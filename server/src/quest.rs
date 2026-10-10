use std::sync::Arc;

use actix_web::{
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::{
    economy::{RewardGrant, RewardItem, apply_reward, grant_chest, is_currency},
    features::repair::RepairData,
    user_data::{
        CompleteCharacterWithIdWithoutData, CompleteInventoryUpdate, CompleteWallet,
        DungeonGeneratedData, DungeonGeneratedDataWithId, InventoryChangeTracker, Item,
        ItemPropertiesAll, QuestWithId, STORY_QUEST_DIFFICULTY_LEVEL,
    },
    util::quest::{GenerateQuestDataError, generate_quest_data},
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper, associations::HasTable, insert_into};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    json_db::JsonDbWrapper,
    models::{
        CharacterDbEntryCharacterAlone, CharacterDbEntryEconomy, QuestDbEntry, QuestDbEntryInfo,
    },
    session::SessionLookedUpMaybe,
    util,
};

/// Which id keys this quest in `character.completedQuests`.
///
/// An EVENT quest is keyed by its own instance id; everything else by its
/// template (`gldQuestId`). For an ordinary quest the two are equal, so the
/// distinction is invisible there — and that is precisely why keying everything
/// by `gldQuestId` looked right for so long.
///
/// Measured over 773 captured retail `/quests` bodies:
///
/// ```text
///   event gldQuestIds appearing as a completedQuests key      0 / 39
///   event INSTANCE questIds appearing as a key              110 / 1071
///   CONTROL - ordinary quests, gldQuestId as key             71 / 71
///   CONTROL - ordinary quests, questId as key                71 / 71
/// ```
///
/// The two controls are the argument: the probe finds ordinary quests under
/// either reading, so the zero for event templates is a real absence and not a
/// broken probe.
///
/// The value stored under this key is the tier counter the client checkmarks
/// from — 1..5 over 185 samples, every one of those rows carrying exactly 5
/// tiers. Written to the wrong key, rewards paid and `event_completions`
/// advanced while the client was told nothing at all. Report #166.
fn completed_quests_key(quest: &blades_lib::user_data::Quest, instance_id: Uuid) -> String {
    if matches!(quest.r#type, blades_lib::user_data::QuestType::GameEvent) {
        instance_id.to_string()
    } else {
        quest.gld_quest_id.to_string()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetQuestsResponse {
    quests: Vec<QuestWithId>,
    dungeon_generated_data_list: Vec<DungeonGeneratedDataWithId>,
    /// Town-job board entries (`type:"JOB"` with a `jobSetup` block), rolled
    /// faithfully from `app_state.job_pools` — see [`jobs_gen`]. Each entry is a
    /// raw `Value` because `jobSetup` carries far more than the typed `Quest`
    /// struct models (it is served verbatim to the client, never re-parsed here).
    jobs: Vec<Value>,
    character: CompleteCharacterWithIdWithoutData,
    /// Per-pool rotation timers (`[{id, endTime, nextStartTime}]`, epoch seconds)
    /// computed relative to *now* by [`jobs_gen`] — no longer frozen constants.
    job_pools: Value,
    /// Quests the server removed in the course of answering this request.
    ///
    /// The job rotation deletes the previous window's un-entered job rows; without
    /// this the client is never told and keeps showing board entries that no longer
    /// exist. Retail sends the field in 17.91% of captured `/quests` responses and
    /// **never sends it empty** — it is a "there were deletions" signal, not a
    /// always-present list — so it is skipped when nothing was removed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deleted_quest_ids: Vec<Uuid>,
    /// The event ("Sigil") quests whose instance window is open right now — one
    /// stored, per-character `GAME_EVENT` row per active event. Their `questId` is a
    /// per-character INSTANCE id and their `gldQuestId` is the template, which is why
    /// everything downstream must resolve through `gldQuestId`.
    game_event_quests: Vec<QuestWithId>,
    /// Events opening within the next 24 h. MEASURED: retail's warning array is
    /// "starting soon", not "ending soon" — all 686 captured entries had a start time
    /// between 0.1 h and 24.0 h in the FUTURE, and there was always exactly one.
    /// These are announcements, so they are not persisted: a warning quest has no
    /// stored progress until its window opens.
    game_event_quests_in_warning: Vec<QuestWithId>,
    /// Deliberately empty. Retail sent 101 entries across the corpus, and the
    /// discriminator is not determinable from it: every one sat 1–48 h after its
    /// instance start — i.e. INSIDE the same 48 h window that the active array uses —
    /// and carried `completed: false`, so it is neither "window elapsed" nor "player
    /// finished it". Sending a guess here would put quests on the player's finished
    /// list that retail would not have. See docs/quest-and-event-model.md.
    game_event_quests_finished: Vec<QuestWithId>,
}

/// Split stored quest rows into the client's `quests[]` and `generatedData[]`.
///
/// Two rows are dropped rather than advertised:
///
/// * **JOB rows**, which are surfaced only in `jobs[]` — matching prod, where the
///   two arrays never overlap. Their `generatedData[]` entries are NOT dropped: retail
///   sends one per job, and the caller adds them from the freshly rolled board via
///   [`jobs_gen::job_generated_data_list`] (see that function for why the board rather
///   than the row is the source).
/// * **Quests with no generated data.** There is no dungeon behind such a quest, so
///   it cannot be placed on the quest map or started. This used to push the quest
///   into `quests[]` anyway while its `generatedData[]` entry was pushed only
///   `if let Some(...)`, handing the client a quest it could list but never resolve:
///   the `!` badge counted it and the map waited forever for data that was never
///   coming. [report #62]
///
///   Measured across all of production: exactly TWO rows were in that state — "The
///   Message" (`cca4a80b…`), whose template ships no `dungeon_uuid` and which is
///   therefore excluded from the built pool, held by exactly the two characters that
///   reported a blank quest map. All 50 other non-job quests carry their data.
///
///   Skipping is the honest answer: with no dungeon we can neither render it nor let
///   anyone play it, and advertising it is what hangs the client.
///
/// A `GAME_EVENT` row is routed to `gameEventQuests[]` instead of `quests[]`, and
/// only while its instance is still open — `open_event_instances` carries the
/// instance ids the event calendar says are live right now. A stored row whose
/// window has closed is simply not advertised (the row stays, so a re-opened window
/// finds the player's milestone progress where they left it).
///
/// The invariant the client relies on, and the one the tests pin: **every quest in
/// `quests[]` or `gameEventQuests[]` has a matching entry in `generatedData[]`,
/// keyed by the same id.** Retail holds it for event quests too — in every captured
/// response the event instance's id was also in `dungeonGeneratedDataList`.
/// Is this row an ordinary quest the player has already completed?
///
/// Strip a job's difficulty off a story quest, in place.
///
/// Returns whether the row changed, so the caller knows to persist it.
///
/// Guards on row kind itself rather than trusting the caller: the same field
/// legitimately holds a real enemy level for a job or an event quest, and only
/// `quests[]` entries take the `-1` sentinel.
fn repair_story_quest_difficulty(info: &mut blades_lib::user_data::Quest) -> bool {
    if jobs_gen::is_job_row(info)
        || matches!(info.r#type, blades_lib::user_data::QuestType::GameEvent)
    {
        return false;
    }
    if info.difficulty_level == STORY_QUEST_DIFFICULTY_LEVEL {
        return false;
    }
    info.difficulty_level = STORY_QUEST_DIFFICULTY_LEVEL;
    true
}

/// "Ordinary" excludes the two row kinds that have their own lifecycle: town jobs
/// (rotated on the daily reset) and event quests (retired when their window
/// closes). What is left is the quest log proper, and a completed entry there is
/// stale — retail serves `completed: false` on every quest it lists.
fn is_finished_ordinary_quest(info: &blades_lib::user_data::Quest) -> bool {
    info.completed
        && !jobs_gen::is_job_row(info)
        && !matches!(info.r#type, blades_lib::user_data::QuestType::GameEvent)
}

fn split_quest_rows(
    rows: impl Iterator<
        Item = (
            Uuid,
            blades_lib::user_data::Quest,
            Option<blades_lib::user_data::DungeonGeneratedData>,
        ),
    >,
    open_event_instances: &std::collections::HashSet<Uuid>,
) -> (
    Vec<QuestWithId>,
    Vec<QuestWithId>,
    Vec<DungeonGeneratedDataWithId>,
) {
    let mut quests = Vec::new();
    let mut event_quests = Vec::new();
    let mut generated = Vec::new();
    for (quest_id, info, generated_data) in rows {
        if jobs_gen::is_job_row(&info) {
            continue;
        }
        let is_event = matches!(info.r#type, blades_lib::user_data::QuestType::GameEvent);
        if is_event && !open_event_instances.contains(&quest_id) {
            continue;
        }
        let Some(inner) = generated_data else {
            continue;
        };
        let with_id = QuestWithId {
            quest_id,
            quest: info,
        };
        if is_event {
            event_quests.push(with_id);
        } else {
            quests.push(with_id);
        }
        generated.push(DungeonGeneratedDataWithId { quest_id, inner });
    }
    (quests, event_quests, generated)
}

/// The response's `dungeonGeneratedDataList`: the stored rows' entries (quests + open
/// events, from [`split_quest_rows`]) plus one for every job on the board.
///
/// Retail's list spans all three — the captured body's 10 entries are 6 jobs + 2 quests
/// + 2 events. Ours omitted the jobs entirely, so the client put them on the map and
/// waited forever for data that never came (report #85, the same failure mode as #62).
///
/// This is a function rather than two lines in the handler so the invariant is testable:
/// the handler itself needs a DB and a session, and the shape-only job tests that let
/// #85 ship are exactly what happens when the assembly step has no test of its own.
fn assemble_generated_data_list(
    mut from_rows: Vec<DungeonGeneratedDataWithId>,
    game_data: &blades_lib::game_data::GameData,
    jobs: &[Value],
    scaling: &blades_lib::static_data::QuestLevelScaling,
) -> Vec<DungeonGeneratedDataWithId> {
    from_rows.extend(jobs_gen::job_generated_data_list(game_data, jobs, scaling));
    from_rows
}

/// Upgrade only the interactable-loot part of a persisted quest generated before the
/// capture-derived tables shipped. Those rows have all the right spawn ids but every
/// `lootTableLoot` result is empty, so keeping the row forever keeps breakables and
/// floor pickups empty forever too (report #152).
///
/// Do not replace the whole generated-data object: an entered quest may already carry
/// enemy/chest state authored by retail or imported with the character. The fresh item
/// map is safe because its rolls are deterministic for the dungeon + spawn ids.
fn refresh_empty_item_loot(stored: &mut DungeonGeneratedData, fresh: DungeonGeneratedData) -> bool {
    let has_item_loot = |data: &DungeonGeneratedData| {
        data.item_generated_data.values().flatten().any(|item| {
            item.loot_table_loot.values().any(|loot| {
                !loot.stackable_items.is_empty()
                    || !loot.currencies.is_empty()
                    || !loot.item.is_empty()
            })
        })
    };

    // Server-generated story rows use version 0. Retail/imported generated data is
    // version 1 and must remain byte-for-byte the player's captured state.
    if stored.version != 0
        || stored.item_generated_data.is_empty()
        || has_item_loot(stored)
        || !has_item_loot(&fresh)
    {
        return false;
    }

    stored.item_generated_data = fresh.item_generated_data;
    true
}

/// Add loot tables a stored row predates (#192).
///
/// [`refresh_empty_item_loot`] repairs a row whose item loot is ENTIRELY empty.
/// It cannot help one that is only partly stale, and after the per-spawn table
/// corpus shipped that is the common case: a Wizard's Challenge pot that already
/// rolls Lumber looks perfectly healthy while the key table beside it is simply
/// absent. Without this, a player who had already accepted the quest would keep
/// keyless pots forever and the only way out would be abandoning it.
///
/// Only tables the stored row LACKS are added. An existing roll is never
/// touched, so nothing the player has already seen changes under them, and
/// spawns absent from the stored row are left alone rather than grown.
fn add_missing_item_tables(
    stored: &mut DungeonGeneratedData,
    fresh: &DungeonGeneratedData,
) -> bool {
    // Retail/imported generated data is version 1 and must stay byte-for-byte
    // the player's captured state.
    if stored.version != 0 {
        return false;
    }
    let mut changed = false;
    for (spawn, fresh_results) in &fresh.item_generated_data {
        let Some(stored_results) = stored.item_generated_data.get_mut(spawn) else {
            continue;
        };
        for (index, fresh_result) in fresh_results.iter().enumerate() {
            let Some(stored_result) = stored_results.get_mut(index) else {
                continue;
            };
            for (table, loot) in &fresh_result.loot_table_loot {
                if !stored_result.loot_table_loot.contains_key(table) {
                    stored_result.loot_table_loot.insert(*table, loot.clone());
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Give a stored row's short floor piles the results they are missing (#353,
/// #358, #365).
///
/// A container spawn retail was never captured on used to get ONE result, though
/// the client places the APK's `_quantity` of them: the uncaptured event dungeons
/// (EQ30, EQ40 and seven more) went out with their 7 / 3 / 1 breakables as 1 / 1 / 1.
/// The client draws every container's contents from this row, and an event row is
/// durable for its whole 48-hour window, so a fixed generator alone would leave
/// the players already holding one with empty containers until the next window.
///
/// Only results past the stored length are appended, from the row's own fresh
/// generation; every result the player may already have opened stays as it was.
/// Rows this server did not generate (`version != 0`) are left alone.
fn grow_short_item_piles(stored: &mut DungeonGeneratedData, fresh: &DungeonGeneratedData) -> bool {
    if stored.version != 0 {
        return false;
    }
    let mut changed = false;
    for (spawn, fresh_results) in &fresh.item_generated_data {
        let Some(stored_results) = stored.item_generated_data.get_mut(spawn) else {
            continue;
        };
        if stored_results.len() < fresh_results.len() {
            stored_results.extend_from_slice(&fresh_results[stored_results.len()..]);
            changed = true;
        }
    }
    changed
}

/// Put a stored EVENT row's enemies at the levels the event dungeon gives them.
///
/// Retail stood each event enemy at the row's `difficultyLevel` plus its spawn
/// group's APK level delta (1,641 of 1,643 captured event enemies; bosses
/// typically +6), and the enemy's variant -- its name, damage and resistances --
/// is picked by the client from that level. Rows minted before this stood every
/// enemy at the bare difficulty. Only `enemyLevel` and `givenXP` move; loot
/// stays as rolled. Rows this server did not generate are left alone.
fn relevel_event_enemies(stored: &mut DungeonGeneratedData, fresh: &DungeonGeneratedData) -> bool {
    if stored.version != 0 {
        return false;
    }
    let mut changed = false;
    for (group, fresh_spawners) in &fresh.enemy_generated_data {
        let Some(stored_spawners) = stored.enemy_generated_data.get_mut(group) else {
            continue;
        };
        for (stored_enemies, fresh_enemies) in stored_spawners.iter_mut().zip(fresh_spawners) {
            for (enemy, want) in stored_enemies.iter_mut().zip(fresh_enemies) {
                if enemy.enemy_level != want.enemy_level || enemy.given_xp != want.given_xp {
                    enemy.enemy_level = want.enemy_level;
                    enemy.given_xp = want.given_xp;
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Add generated dungeon sections a stored row predates (#260).
///
/// Retail generated every stage of a multi-part quest at accept time. Rows
/// minted before we knew that can be missing an entire sibling dungeon's spawn
/// groups, so later repairs that only touch existing groups can never add the
/// key-holder. Only absent spawn groups are inserted; existing rolls stay as the
/// player already saw them, and captured/imported version-1 rows are left alone.
fn add_missing_dungeon_sections(
    stored: &mut DungeonGeneratedData,
    fresh: &DungeonGeneratedData,
) -> bool {
    if stored.version != 0 {
        return false;
    }

    let mut changed = false;
    for (group, enemies) in &fresh.enemy_generated_data {
        if !stored.enemy_generated_data.contains_key(group) {
            stored.enemy_generated_data.insert(*group, enemies.clone());
            changed = true;
        }
    }
    for (spawn, items) in &fresh.item_generated_data {
        if !stored.item_generated_data.contains_key(spawn) {
            stored.item_generated_data.insert(*spawn, items.clone());
            changed = true;
        }
    }
    for (spawn, chests) in &fresh.chest_generated_data {
        if !stored.chest_generated_data.contains_key(spawn) {
            stored.chest_generated_data.insert(*spawn, chests.clone());
            changed = true;
        }
    }
    changed
}

/// What a stored EVENT row is repaired against: its event's dungeon, every stage,
/// at the row's own `difficultyLevel` — the level the client was shown — rather
/// than the story path's fresh data at the player's current level.
///
/// Event rows minted before report #323 carry only the first stage of a
/// multi-stage event, and `/dungeons/current/exit` handed the client the same
/// first-stage-only data for the next run. The row is durable for the event's
/// window, so without this the player would play every run of that event with
/// later stages that show no experience, drop nothing and hold empty containers.
fn event_row_fresh(
    game_data: &blades_lib::game_data::GameData,
    scaling: &blades_lib::static_data::QuestLevelScaling,
    row_id: Uuid,
    info: &blades_lib::user_data::Quest,
) -> Option<DungeonGeneratedData> {
    crate::dungeon::event_dungeon_data_for_run(
        game_data,
        info.gld_quest_id,
        info.difficulty_level,
        scaling,
        row_id,
        0,
    )
    .ok()
    .map(|(_, data)| data)
}

/// Take out of a stored EVENT row every spawn group, chest and container of a
/// stage its event never had (#329).
///
/// #465 gave The Web Mother's Trap (EQ23) the `_B` stage of the story cave it is
/// built on, and its `/quests` repair added `_B` to every EQ23 row already
/// minted: 10 live rows on 2026-10-03. Retail never served that stage, and the one
/// player who ran such a row found the Wispmothers and the spider boss already
/// dead. The row is durable for the event's 48-hour window, so it is healed here
/// rather than left to the next mint. `_A`'s rolls are untouched; rows this server
/// did not generate (`version != 0`) are left alone, as the other repairs leave them.
fn drop_foreign_event_stages(
    game_data: &blades_lib::game_data::GameData,
    info: &blades_lib::user_data::Quest,
    stored: &mut DungeonGeneratedData,
) -> bool {
    if stored.version != 0 {
        return false;
    }
    let Some(dungeon_uuid) = game_data
        .quests
        .get(&info.gld_quest_id)
        .and_then(|q| q.dungeon_info.as_ref())
        .map(|d| d.dungeon_uuid)
    else {
        return false;
    };
    let mut changed = false;
    for foreign in blades_lib::util::quest::event_dungeon_foreign_stage_ids(game_data, &dungeon_uuid) {
        let spawn = &game_data.dungeons[&foreign].spawn_info;
        for group in spawn.enemy_spawn_groups.keys() {
            changed |= stored.enemy_generated_data.remove(group).is_some();
        }
        for container in spawn.item.keys() {
            changed |= stored.item_generated_data.remove(container).is_some();
        }
        for chest in spawn.chest.keys() {
            changed |= stored.chest_generated_data.remove(chest).is_some();
        }
    }
    changed
}

/// Give a stored row the key a key-holder enemy should carry (#236).
///
/// Rows generated before report #236 have the right enemies but no key: the
/// Mercenary's `spawnGroupLoot` was always `{}`, and a key-holder rolling the
/// DoorKey table got an empty result for it. Both rows are durable -- an event
/// quest is minted once per window -- so without this the player who accepted
/// one keeps a keyless Mercenary until the window closes.
///
/// Only what was empty BY CONSTRUCTION is filled: an empty `spawnGroupLoot`
/// where the fresh roll has one, and an empty result for a table the enemy
/// corpus does not model (a modelled table rolling empty is an ordinary outcome
/// and stays empty). Nothing already rolled is rewritten, enemies absent from
/// the stored row are not grown, and version-1 rows stay byte-for-byte.
///
/// A row whose enemies carry no loot at all predates enemy loot entirely, and
/// `dungeon_update` credits its corpses from the client's request instead;
/// giving one of its enemies loot would switch every other corpse in it to
/// crediting nothing, so such a row is left alone.
fn add_missing_enemy_key_loot(
    stored: &mut DungeonGeneratedData,
    fresh: &DungeonGeneratedData,
) -> bool {
    use blades_lib::util::dungeon::enemy_table_is_unmodelled;

    if stored.version != 0 {
        return false;
    }
    let has_enemy_loot = stored
        .enemy_generated_data
        .values()
        .flatten()
        .flatten()
        .any(|e| !e.loot_table_loot.is_empty() || !e.spawn_group_loot.is_empty());
    if !has_enemy_loot {
        return false;
    }

    let mut changed = false;
    for (group, fresh_spawners) in &fresh.enemy_generated_data {
        let Some(stored_spawners) = stored.enemy_generated_data.get_mut(group) else {
            continue;
        };
        for (fresh_enemies, stored_enemies) in fresh_spawners.iter().zip(stored_spawners.iter_mut())
        {
            for (fresh_enemy, stored_enemy) in fresh_enemies.iter().zip(stored_enemies.iter_mut()) {
                if stored_enemy.spawn_group_loot.is_empty()
                    && !fresh_enemy.spawn_group_loot.is_empty()
                {
                    stored_enemy.spawn_group_loot = fresh_enemy.spawn_group_loot.clone();
                    changed = true;
                }
                for (table, fresh_loot) in &fresh_enemy.loot_table_loot {
                    if fresh_loot.is_empty() || !enemy_table_is_unmodelled(table) {
                        continue;
                    }
                    if let Some(stored_loot) = stored_enemy.loot_table_loot.get_mut(table) {
                        if stored_loot.is_empty() {
                            *stored_loot = fresh_loot.clone();
                            changed = true;
                        }
                    }
                }
            }
        }
    }
    changed
}

/// Give a stored row the enemies its short spawners are missing (#301).
///
/// Rows generated before #301 hold ONE enemy at every spawner, where the APK
/// (and retail) put up to six. The client spawns exactly what the row lists, so
/// "The Troll Trap" -- whose objective counts the three trolls of one spawner --
/// stays unfinishable for whoever accepted it until the row itself grows.
///
/// Only enemies are APPENDED, at the end of a spawner shorter than the fresh
/// one: every stored enemy keeps its index, level, XP and loot, so a kill the
/// player already reported still names the same enemy. The new ones take their
/// spawner's stored level and XP (retail levelled a spawner as one), loot from
/// the fresh roll at their own index, and never `spawnGroupLoot`, which retail
/// only ever put on enemy zero. Version-1 rows stay byte-for-byte.
///
/// A row whose enemies carry no loot at all predates enemy loot, and
/// `dungeon_update` credits its corpses from the client's request; the new
/// enemies join it loot-less so that stays true for the whole row.
fn add_missing_spawner_enemies(
    stored: &mut DungeonGeneratedData,
    fresh: &DungeonGeneratedData,
) -> bool {
    if stored.version != 0 {
        return false;
    }
    let has_enemy_loot = stored
        .enemy_generated_data
        .values()
        .flatten()
        .flatten()
        .any(|e| !e.loot_table_loot.is_empty() || !e.spawn_group_loot.is_empty());

    let mut changed = false;
    for (group, fresh_spawners) in &fresh.enemy_generated_data {
        let Some(stored_spawners) = stored.enemy_generated_data.get_mut(group) else {
            continue;
        };
        for (fresh_enemies, stored_enemies) in fresh_spawners.iter().zip(stored_spawners.iter_mut())
        {
            // Not `.first()`: diesel's `first` is in scope here and shadows it.
            let Some((enemy_level, given_xp)) = stored_enemies[..]
                .iter()
                .next()
                .map(|e| (e.enemy_level, e.given_xp))
            else {
                continue;
            };
            let have = stored_enemies.len();
            for fresh_enemy in fresh_enemies.iter().skip(have) {
                stored_enemies.push(blades_lib::user_data::DungeonEnemyResult {
                    enemy_level,
                    given_xp,
                    spawn_group_loot: Default::default(),
                    loot_table_loot: if has_enemy_loot {
                        fresh_enemy.loot_table_loot.clone()
                    } else {
                        Default::default()
                    },
                });
                changed = true;
            }
        }
    }
    changed
}

/// The shipped `quests_daily.json` scaling — the same table the server loads.
///
/// Tests reached for `QuestLevelScaling::default()`, which is EMPTY and therefore
/// exercises the last-resort `100 * level` formula rather than the real one. That
/// is exactly how the job board kept paying the old XP after the real numbers moved
/// into the file: every test agreed with the bug, because every test was asking the
/// fallback.
#[cfg(test)]
pub(crate) fn shipped_scaling() -> blades_lib::static_data::QuestLevelScaling {
    let p =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static/quests_daily.json");
    let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
    let json: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
    let scaling: blades_lib::static_data::QuestLevelScaling =
        serde_json::from_value(json["levelScaling"].clone()).expect("levelScaling parses");
    assert!(
        !scaling.measured.given_xp_by_enemy_level.is_empty(),
        "the shipped scaling parsed to an empty table — the tests would silently \
         fall back to the old formula and agree with any regression"
    );
    scaling
}

#[cfg(test)]
mod report236_key_holder_repair_tests {
    use super::*;

    /// "A Battle Unceasing" (EQ15), the event quest of the report.
    const EQ15_QUEST: &str = "e8f3614c-8672-4f77-9dad-4b400676f4b6";
    const MERCENARY: &str = "a91ebfe6-0167-4643-bd6f-ed27d8dfad41";
    /// EQ13's quest; its key-holder rolls the DoorKey TABLE instead.
    const EQ13_QUEST: &str = "31baf922-2b1a-4605-ae52-4946e26994c6";
    const EQ13_KEY_HOLDER: &str = "101b2285-0679-4d5c-92d5-a34045f145ce";
    const KEY_TABLE: &str = "8858f284-4f33-4da4-8085-0befa7ef2637";
    const DOOR_KEY: &str = "faa3aeb3-9284-4d83-8981-1af00e3a6398";

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    fn fresh(quest: &str, level: i64) -> DungeonGeneratedData {
        let game_data = super::report85_job_generated_data_tests::game_data();
        let (_, generated) = generate_quest_data(
            &game_data,
            uuid(quest),
            level,
            &blades_lib::static_data::QuestLevelScaling::default(),
        )
        .expect("the quest exists");
        generated.expect("it has a dungeon")
    }

    /// The row as stored before the fix: no spawnGroupLoot, and the key table
    /// present but empty -- exactly what production holds for RonnieRaider.
    fn as_stored_before_the_fix(fresh: &DungeonGeneratedData) -> DungeonGeneratedData {
        let mut stored = fresh.clone();
        for enemy in stored.enemy_generated_data.values_mut().flatten().flatten() {
            enemy.spawn_group_loot = Default::default();
            if let Some(l) = enemy.loot_table_loot.get_mut(&uuid(KEY_TABLE)) {
                *l = Default::default();
            }
        }
        stored
    }

    fn keys(data: &DungeonGeneratedData, group: &str) -> u64 {
        data.enemy_generated_data[&uuid(group)]
            .iter()
            .flatten()
            .filter_map(|e| {
                e.merged_loot_table()
                    .stackable_items
                    .get(&uuid(DOOR_KEY))
                    .copied()
            })
            .sum()
    }

    /// THE REPAIR, for both roads: an accepted EQ15 gains the Mercenary's key,
    /// and an accepted EQ13 its key-holder's.
    #[test]
    fn an_accepted_quest_gains_its_key_holders_key() {
        for (quest, holder) in [(EQ15_QUEST, MERCENARY), (EQ13_QUEST, EQ13_KEY_HOLDER)] {
            // Stored at one level, refreshed at another: the player levelled.
            let mut stored = as_stored_before_the_fix(&fresh(quest, 73));
            assert_eq!(keys(&stored, holder), 0, "{quest}: the precondition");
            assert!(
                add_missing_enemy_key_loot(&mut stored, &fresh(quest, 89)),
                "{quest}"
            );
            assert_eq!(keys(&stored, holder), 1, "{quest}: still no key");
        }
    }

    /// CONTROL: nothing already rolled changes -- not the gold the player may
    /// have seen, not an empty roll of a table the corpus DOES model, not the
    /// items or chests.
    #[test]
    fn it_fills_only_what_was_empty_by_construction() {
        let mut stored = as_stored_before_the_fix(&fresh(EQ15_QUEST, 73));
        let gold = uuid("871c2e9b-7e7a-4564-a022-e435dfb8a436");
        // A modelled table that rolled empty must stay empty.
        let some_enemy = stored
            .enemy_generated_data
            .get_mut(&uuid("7461fd2c-c417-4b80-b185-6d491982787e"))
            .unwrap()[0][0]
            .loot_table_loot
            .get_mut(&gold)
            .unwrap();
        *some_enemy = Default::default();
        let before = serde_json::to_value(&stored).unwrap();

        assert!(add_missing_enemy_key_loot(
            &mut stored,
            &fresh(EQ15_QUEST, 73)
        ));

        let mut after = serde_json::to_value(&stored).unwrap();
        // Take the one intended change back out; everything else must be equal.
        after["enemyGeneratedData"][MERCENARY][0][0]["spawnGroupLoot"] = serde_json::json!({});
        assert_eq!(after, before);
    }

    /// CONTROL: retail and imported rows are version 1 and stay byte-for-byte.
    #[test]
    fn it_leaves_captured_rows_alone() {
        let mut stored = as_stored_before_the_fix(&fresh(EQ15_QUEST, 30));
        stored.version = 1;
        let before = serde_json::to_value(&stored).unwrap();
        assert!(!add_missing_enemy_key_loot(
            &mut stored,
            &fresh(EQ15_QUEST, 30)
        ));
        assert_eq!(serde_json::to_value(&stored).unwrap(), before);
    }

    /// CONTROL: a row from before enemy loot existed is left alone. Its corpses
    /// are credited from the client's request; one enemy with loot would switch
    /// every other corpse in it to crediting nothing.
    #[test]
    fn a_row_that_predates_enemy_loot_is_left_alone() {
        let mut stored = fresh(EQ15_QUEST, 30);
        for enemy in stored.enemy_generated_data.values_mut().flatten().flatten() {
            enemy.spawn_group_loot = Default::default();
            enemy.loot_table_loot.clear();
        }
        assert!(!add_missing_enemy_key_loot(
            &mut stored,
            &fresh(EQ15_QUEST, 30)
        ));
        assert_eq!(keys(&stored, MERCENARY), 0);
    }

    /// CONTROL: an up-to-date row reports no change, so /quests does not write
    /// the same bytes back on every poll.
    #[test]
    fn an_up_to_date_row_is_not_rewritten() {
        let mut stored = fresh(EQ15_QUEST, 30);
        assert!(!add_missing_enemy_key_loot(
            &mut stored,
            &fresh(EQ15_QUEST, 30)
        ));
    }
}

#[cfg(test)]
mod report301_spawner_repair_tests {
    use super::*;

    /// "The Troll Trap", and the group its kill objective counts.
    const TROLL_TRAP: &str = "45c042e1-538b-4079-8b92-06f1d7677b6f";
    const TROLLS: &str = "d971b82c-7113-4c95-84da-cbb2a4ad29f3";
    const SPIDERS: [&str; 2] = [
        "d8f00672-6937-410c-b17c-11a91aedded2",
        "4e41da9a-dd8d-4a3b-8be5-d7c99e86beb8",
    ];

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    fn fresh(level: i64) -> DungeonGeneratedData {
        let game_data = super::report85_job_generated_data_tests::game_data();
        let (_, generated) = generate_quest_data(
            &game_data,
            uuid(TROLL_TRAP),
            level,
            &blades_lib::static_data::QuestLevelScaling::default(),
        )
        .expect("the quest exists");
        generated.expect("it has a dungeon")
    }

    /// The row as minted before #301: one enemy at every spawner.
    fn as_stored_before_the_fix(fresh: &DungeonGeneratedData) -> DungeonGeneratedData {
        let mut stored = fresh.clone();
        for enemies in stored.enemy_generated_data.values_mut().flatten() {
            enemies.truncate(1);
        }
        stored
    }

    fn lens(data: &DungeonGeneratedData, group: &str) -> Vec<usize> {
        data.enemy_generated_data[&uuid(group)]
            .iter()
            .map(Vec::len)
            .collect()
    }

    /// THE REPAIR: an accepted Troll Trap grows its one troll to three, keeping
    /// the troll the player already had exactly as it was, and the new two at
    /// its level even though the player has levelled since.
    #[test]
    fn an_accepted_troll_trap_gains_its_missing_trolls() {
        let mut stored = as_stored_before_the_fix(&fresh(14));
        assert_eq!(lens(&stored, TROLLS), vec![1], "the precondition");
        let kept = serde_json::to_value(&stored.enemy_generated_data[&uuid(TROLLS)][0][0]).unwrap();

        let refreshed_at = fresh(30);
        assert_ne!(
            refreshed_at.enemy_generated_data[&uuid(TROLLS)][0][1].enemy_level,
            stored.enemy_generated_data[&uuid(TROLLS)][0][0].enemy_level,
            "the precondition: the refresh is at a different level"
        );
        assert!(add_missing_spawner_enemies(&mut stored, &refreshed_at));

        assert_eq!(lens(&stored, TROLLS), vec![3]);
        for spiders in SPIDERS {
            assert_eq!(lens(&stored, spiders), vec![2], "{spiders}");
        }
        let trolls = &stored.enemy_generated_data[&uuid(TROLLS)][0];
        assert_eq!(serde_json::to_value(&trolls[0]).unwrap(), kept);
        for (i, troll) in trolls.iter().enumerate().skip(1) {
            assert_eq!(
                (troll.enemy_level, troll.given_xp),
                (trolls[0].enemy_level, trolls[0].given_xp),
                "troll {i} took the refresh's level, not its spawner's"
            );
            assert!(troll.spawn_group_loot.is_empty());
            assert_eq!(
                serde_json::to_value(&troll.loot_table_loot).unwrap(),
                serde_json::to_value(
                    &refreshed_at.enemy_generated_data[&uuid(TROLLS)][0][i].loot_table_loot
                )
                .unwrap(),
                "troll {i} carries its own roll"
            );
        }
    }

    /// CONTROL: a second /quests poll is a no-op, so the row is not rewritten
    /// on every refresh.
    #[test]
    fn a_second_run_is_a_no_op() {
        let mut stored = as_stored_before_the_fix(&fresh(14));
        assert!(add_missing_spawner_enemies(&mut stored, &fresh(14)));
        let once = serde_json::to_value(&stored).unwrap();
        assert!(!add_missing_spawner_enemies(&mut stored, &fresh(14)));
        assert_eq!(serde_json::to_value(&stored).unwrap(), once);
    }

    /// CONTROL: a row minted with the fix is already full and stays as it is.
    #[test]
    fn a_full_row_is_left_alone() {
        let mut stored = fresh(14);
        let before = serde_json::to_value(&stored).unwrap();
        assert!(!add_missing_spawner_enemies(&mut stored, &fresh(30)));
        assert_eq!(serde_json::to_value(&stored).unwrap(), before);
    }

    /// CONTROL: retail and imported rows are version 1 and stay byte-for-byte.
    #[test]
    fn captured_rows_are_left_alone() {
        let mut stored = as_stored_before_the_fix(&fresh(14));
        stored.version = 1;
        let before = serde_json::to_value(&stored).unwrap();
        assert!(!add_missing_spawner_enemies(&mut stored, &fresh(14)));
        assert_eq!(serde_json::to_value(&stored).unwrap(), before);
    }

    /// A row from before enemy loot gains its trolls WITHOUT loot: one looted
    /// corpse would switch every other corpse in it to crediting nothing.
    #[test]
    fn a_row_that_predates_enemy_loot_gains_loot_less_enemies() {
        let mut stored = as_stored_before_the_fix(&fresh(14));
        for enemy in stored.enemy_generated_data.values_mut().flatten().flatten() {
            enemy.spawn_group_loot = Default::default();
            enemy.loot_table_loot.clear();
        }
        assert!(add_missing_spawner_enemies(&mut stored, &fresh(14)));
        assert_eq!(lens(&stored, TROLLS), vec![3]);
        assert!(
            stored
                .enemy_generated_data
                .values()
                .flatten()
                .flatten()
                .all(|e| e.loot_table_loot.is_empty() && e.spawn_group_loot.is_empty())
        );
    }
}

#[cfg(test)]
mod report192_missing_table_tests {
    use super::*;

    const WIZARDS_CHALLENGE_QUEST: &str = "334e582f-95ba-4263-b381-ac6d91eabe92";
    const KEY_TABLE: &str = "8858f284-4f33-4da4-8085-0befa7ef2637";
    const KEY_POT: &str = "9f2a4d7d-debf-457f-8007-19a0e40dfb0c";

    fn fresh_wizards_challenge() -> DungeonGeneratedData {
        let game_data = super::report85_job_generated_data_tests::game_data();
        let quest_id = Uuid::parse_str(WIZARDS_CHALLENGE_QUEST).unwrap();
        let (_, generated) = generate_quest_data(
            &game_data,
            quest_id,
            48,
            &blades_lib::static_data::QuestLevelScaling::default(),
        )
        .expect("The Wizard's Challenge exists");
        generated.expect("it has a dungeon")
    }

    /// The stored row shape of a player who accepted the quest before the
    /// per-spawn corpus shipped: the pots roll Lumber and nothing else.
    fn as_stored_before_the_fix(fresh: &DungeonGeneratedData) -> DungeonGeneratedData {
        let key_table = Uuid::parse_str(KEY_TABLE).unwrap();
        let mut stored = fresh.clone();
        for results in stored.item_generated_data.values_mut() {
            for result in results {
                result.loot_table_loot.remove(&key_table);
            }
        }
        stored
    }

    /// THE REPAIR. `refresh_empty_item_loot` refuses this row — its item loot is
    /// not empty, the Lumber table pays — so without this the accepted quest
    /// keeps keyless pots and the door never opens.
    #[test]
    fn an_accepted_quest_gains_the_key_table_it_predates() {
        let fresh = fresh_wizards_challenge();
        let mut stored = as_stored_before_the_fix(&fresh);
        let pot = Uuid::parse_str(KEY_POT).unwrap();
        let key_table = Uuid::parse_str(KEY_TABLE).unwrap();

        assert!(
            !refresh_empty_item_loot(&mut stored, fresh.clone()),
            "the old repair must refuse this row — that is why this one exists"
        );
        assert!(
            !stored.item_generated_data[&pot][0]
                .loot_table_loot
                .contains_key(&key_table)
        );

        assert!(add_missing_item_tables(&mut stored, &fresh));
        assert_eq!(
            serde_json::to_value(&stored.item_generated_data).unwrap(),
            serde_json::to_value(&fresh.item_generated_data).unwrap(),
        );
    }

    /// CONTROL: a roll the player may already have seen is never rewritten, and
    /// the enemy and chest sections are not touched at all.
    #[test]
    fn it_adds_and_never_rewrites() {
        let fresh = fresh_wizards_challenge();
        let mut stored = as_stored_before_the_fix(&fresh);
        let pot = Uuid::parse_str(KEY_POT).unwrap();
        let lumber = *stored.item_generated_data[&pot][0]
            .loot_table_loot
            .keys()
            .next()
            .expect("the pot already rolls something");
        // Something only the stored row has: a value the fresh roll disagrees with.
        stored.item_generated_data.get_mut(&pot).unwrap()[0]
            .loot_table_loot
            .get_mut(&lumber)
            .unwrap()
            .stackable_items
            .insert(Uuid::nil(), 99);
        let enemies_before = serde_json::to_value(&stored.enemy_generated_data).unwrap();
        let chests_before = serde_json::to_value(&stored.chest_generated_data).unwrap();

        assert!(add_missing_item_tables(&mut stored, &fresh));

        assert_eq!(
            stored.item_generated_data[&pot][0].loot_table_loot[&lumber]
                .stackable_items
                .get(&Uuid::nil()),
            Some(&99),
            "the existing roll was rewritten"
        );
        assert_eq!(
            serde_json::to_value(&stored.enemy_generated_data).unwrap(),
            enemies_before
        );
        assert_eq!(
            serde_json::to_value(&stored.chest_generated_data).unwrap(),
            chests_before
        );
    }

    /// CONTROL: retail and imported rows are version 1 and stay byte-for-byte.
    #[test]
    fn it_leaves_captured_rows_alone() {
        let fresh = fresh_wizards_challenge();
        let mut stored = as_stored_before_the_fix(&fresh);
        stored.version = 1;
        let before = serde_json::to_value(&stored).unwrap();
        assert!(!add_missing_item_tables(&mut stored, &fresh));
        assert_eq!(serde_json::to_value(&stored).unwrap(), before);
    }

    /// CONTROL: a row that already has everything reports no change, so the
    /// /quests refresh does not write the same bytes back on every poll.
    #[test]
    fn an_up_to_date_row_is_not_rewritten() {
        let fresh = fresh_wizards_challenge();
        let mut stored = fresh.clone();
        assert!(!add_missing_item_tables(&mut stored, &fresh));
    }
}

#[cfg(test)]
mod report260_variant_family_repair_tests {
    use super::*;

    const POOL_OF_DESPAIR: &str = "c178813f-ea7c-4732-aba0-a5c8d8767ec9";
    const SQ201_2: &str = "0ded6e84-d942-434b-8563-33ef301f6189";
    const SQ201_KEY_HOLDER: &str = "379775a2-8015-4c8c-a503-0691597528a0";
    const DOOR_KEY: &str = "faa3aeb3-9284-4d83-8981-1af00e3a6398";

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    fn fresh(level: i64) -> DungeonGeneratedData {
        let game_data = super::report85_job_generated_data_tests::game_data();
        let (_, generated) = generate_quest_data(
            &game_data,
            uuid(POOL_OF_DESPAIR),
            level,
            &blades_lib::static_data::QuestLevelScaling::default(),
        )
        .expect("Pool of Despair exists");
        generated.expect("it has a dungeon")
    }

    fn remove_sq201_stage_2(data: &mut DungeonGeneratedData) {
        let game_data = super::report85_job_generated_data_tests::game_data();
        let stage_2 = &game_data.dungeons[&uuid(SQ201_2)];
        for group in stage_2.spawn_info.enemy_spawn_groups.keys() {
            data.enemy_generated_data.remove(group);
        }
        for spawn in stage_2.spawn_info.item.keys() {
            data.item_generated_data.remove(spawn);
        }
        for spawn in stage_2.spawn_info.chest.keys() {
            data.chest_generated_data.remove(spawn);
        }
    }

    fn keys(data: &DungeonGeneratedData) -> u64 {
        data.enemy_generated_data
            .get(&uuid(SQ201_KEY_HOLDER))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.merged_loot_table()
                    .stackable_items
                    .get(&uuid(DOOR_KEY))
                    .copied()
            })
            .sum()
    }

    #[test]
    fn accepted_pool_rows_gain_the_missing_second_stage_and_key_holder() {
        let fresh = fresh(13);
        let mut stored = fresh.clone();
        remove_sq201_stage_2(&mut stored);
        let before = serde_json::to_value(&stored).unwrap();
        assert_eq!(keys(&stored), 0, "the pre-fix row lacks the key-holder");

        assert!(add_missing_dungeon_sections(&mut stored, &fresh));

        assert_eq!(
            keys(&stored),
            1,
            "the SQ201 stage-2 key-holder was not restored"
        );
        let mut after = serde_json::to_value(&stored).unwrap();
        for section in [
            "enemyGeneratedData",
            "itemGeneratedData",
            "chestGeneratedData",
        ] {
            let Some(obj) = after[section].as_object_mut() else {
                continue;
            };
            let Some(fresh_obj) = serde_json::to_value(&fresh).unwrap()[section]
                .as_object()
                .cloned()
            else {
                continue;
            };
            obj.retain(|id, _| !fresh_obj.contains_key(id) || before[section].get(id).is_some());
        }
        assert_eq!(after, before, "existing generated data was rewritten");
    }

    #[test]
    fn captured_rows_do_not_grow_missing_variant_stages() {
        let fresh = fresh(13);
        let mut stored = fresh.clone();
        remove_sq201_stage_2(&mut stored);
        stored.version = 1;
        let before = serde_json::to_value(&stored).unwrap();

        assert!(!add_missing_dungeon_sections(&mut stored, &fresh));
        assert_eq!(serde_json::to_value(&stored).unwrap(), before);
    }
}

#[cfg(test)]
mod report329_foreign_event_stage_tests {
    use super::*;

    /// The Web Mother's Trap and its two dungeons; EQ24 is the control, whose `_B`
    /// IS a stage of the event (#323).
    const EQ23: &str = "2d1200ee-ecb8-4ea6-9892-54d1f538d83d";
    const EQ23_A: &str = "401ffa22-79ba-4c1c-aaa2-d67730d76aad";
    const EQ23_B: &str = "22d33501-f0df-4d0c-96e1-22da7f535ba8";
    const EQ24: &str = "816ff4c8-b56f-4645-bd2a-29bc7c1baf96";
    const EQ24_A: &str = "88d3d9f4-fb63-4d2a-af60-66ef1ba74736";

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    fn event_row(
        game_data: &blades_lib::game_data::GameData,
        quest: &str,
        level: i64,
    ) -> blades_lib::user_data::Quest {
        let (mut info, _) = generate_quest_data(
            game_data,
            uuid(quest),
            level,
            &blades_lib::static_data::QuestLevelScaling::default(),
        )
        .unwrap();
        info.r#type = blades_lib::user_data::QuestType::GameEvent;
        info
    }

    /// Raysiel's row on prod (#329): minted with `_A` on 2026-10-02, then given
    /// `_B` by #465's `/quests` repair — 25 groups, 2 chests, 6 containers, all at
    /// the row's level 2. Nine other players hold the same row.
    ///
    /// The repair takes `_B` back out and leaves every `_A` roll as it was shown.
    #[test]
    fn an_eq23_row_given_the_story_stage_loses_it_and_keeps_its_own() {
        let gd = super::report85_job_generated_data_tests::game_data();
        let info = event_row(&gd, EQ23, 2);
        let a_only = blades_lib::util::dungeon::generate_for_dungeon_with_seed(
            &gd,
            &uuid(EQ23_A),
            7,
            2,
            13,
        )
        .unwrap();
        let mut stored = a_only.clone();
        let b = blades_lib::util::dungeon::generate_for_dungeon(&gd, &uuid(EQ23_B), 2, 13).unwrap();
        blades_lib::util::quest::merge_dungeon_generated_data(&mut stored, b);
        assert_eq!(
            (
                stored.enemy_generated_data.len(),
                stored.chest_generated_data.len(),
                stored.item_generated_data.len()
            ),
            (25, 2, 6),
            "the shape of the prod row"
        );

        assert!(drop_foreign_event_stages(&gd, &info, &mut stored));
        assert_eq!(
            serde_json::to_value(&stored).unwrap(),
            serde_json::to_value(&a_only).unwrap(),
            "exactly `_A`, every roll as the player was shown it"
        );
        assert!(!drop_foreign_event_stages(&gd, &info, &mut stored), "nothing left to drop");
    }

    /// CONTROL: an EQ24 row keeps its `_B` — that one is a stage of the event —
    /// and a row the server did not generate is not touched.
    #[test]
    fn a_real_second_stage_and_a_captured_row_are_left_alone() {
        let gd = super::report85_job_generated_data_tests::game_data();
        let eq24 = event_row(&gd, EQ24, 16);
        let mut both =
            blades_lib::util::quest::generate_for_event_dungeon(
                &gd,
                &uuid(EQ24_A),
                16,
                &blades_lib::static_data::QuestLevelScaling::default(),
            )
            .unwrap();
        let before = serde_json::to_value(&both).unwrap();
        assert_eq!(both.enemy_generated_data.len(), 20);
        assert!(!drop_foreign_event_stages(&gd, &eq24, &mut both));
        assert_eq!(serde_json::to_value(&both).unwrap(), before);

        let eq23 = event_row(&gd, EQ23, 2);
        let mut captured =
            blades_lib::util::dungeon::generate_for_dungeon(&gd, &uuid(EQ23_B), 2, 13).unwrap();
        captured.version = 1;
        assert!(!drop_foreign_event_stages(&gd, &eq23, &mut captured));
    }
}

#[cfg(test)]
mod report152_stale_story_loot_tests {
    use super::*;
    use blades_lib::static_data::QuestLevelScaling;

    #[test]
    fn haunted_forest_refreshes_old_empty_item_rolls_without_replacing_enemy_data() {
        let game_data = super::report85_job_generated_data_tests::game_data();
        let quest_id = Uuid::parse_str("378307c6-0a23-41f8-b721-5282fa0a8a2b").unwrap();
        let (_, fresh) =
            generate_quest_data(&game_data, quest_id, 48, &QuestLevelScaling::default())
                .expect("Haunted Forest exists");
        let fresh = fresh.expect("Haunted Forest has a dungeon");

        assert_eq!(
            fresh.item_generated_data.len(),
            41,
            "all floor spawns from MQ16_DungeonSettings_A and _B"
        );
        let paying_spawns = fresh
            .item_generated_data
            .values()
            .flatten()
            .filter(|item| {
                item.loot_table_loot.values().any(|loot| {
                    !loot.stackable_items.is_empty()
                        || !loot.currencies.is_empty()
                        || !loot.item.is_empty()
                })
            })
            .count();
        assert_eq!(paying_spawns, 37, "the capture-derived deterministic rolls");

        // Exact shape of the reporter's durable pre-fix row: spawn/table ids exist,
        // but every result is empty. Preserve the enemy/chest sections verbatim.
        let mut stale = fresh.clone();
        for item in stale.item_generated_data.values_mut().flatten() {
            for loot in item.loot_table_loot.values_mut() {
                *loot = Default::default();
            }
        }
        let enemies_before = serde_json::to_value(&stale.enemy_generated_data).unwrap();
        let chests_before = serde_json::to_value(&stale.chest_generated_data).unwrap();

        assert!(refresh_empty_item_loot(&mut stale, fresh.clone()));
        assert_eq!(
            serde_json::to_value(&stale.item_generated_data).unwrap(),
            serde_json::to_value(&fresh.item_generated_data).unwrap(),
        );
        assert_eq!(
            serde_json::to_value(&stale.enemy_generated_data).unwrap(),
            enemies_before,
        );
        assert_eq!(
            serde_json::to_value(&stale.chest_generated_data).unwrap(),
            chests_before,
        );
    }

    #[test]
    fn a_row_that_already_has_loot_is_not_rewritten() {
        let game_data = super::report85_job_generated_data_tests::game_data();
        let quest_id = Uuid::parse_str("378307c6-0a23-41f8-b721-5282fa0a8a2b").unwrap();
        let (_, fresh) =
            generate_quest_data(&game_data, quest_id, 48, &QuestLevelScaling::default())
                .expect("Haunted Forest exists");
        let fresh = fresh.expect("Haunted Forest has a dungeon");
        let mut stored = fresh.clone();

        assert!(!refresh_empty_item_loot(&mut stored, fresh));
    }
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests")]
pub async fn get_quests(
    session: SessionLookedUpMaybe,
    request: Json<Option<()>>,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GetQuestsResponse>, BladeApiError> {
    assert!(request.is_none());
    let session = session.get_session_or_error()?;

    let character_id_var = path.into_inner();
    // Real wall clock — this is a live server, so `now` drives the pool rotation
    // and the daily reset. Tests exercise the pure generator with an injected `now`.
    let now = jobs_gen::now_epoch_secs();
    let job_pools_def = app_state.job_pools.clone();
    // Shared globals for use inside the transaction closure (the daily-quest select reads
    // static_data + game_data). Cloning the Arc avoids borrowing `app_state` across the
    // `conn` borrow (which the closure would otherwise move — E0505).
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();
    conn.transaction(|mut conn| {
        async move {
            // Collected by the job rotation below and reported to the client.
            let mut deleted_quest_ids: Vec<Uuid> = Vec::new();
            let character = {
                use crate::schema::characters::dsl::*;

                characters::table()
                    .filter(id.eq(&character_id_var))
                    .select(CharacterDbEntryCharacterAlone::as_select())
                    .load(&mut conn)
                    .await?
            };
            let mut character =
                util::get_only_single_character_and_check_permission(character, &session.session)?;

            // ---- Town jobs: rotate + (re)generate for the current reset window ----
            // The character carries `lastJobsResetTime`; when it predates the current
            // daily-reset boundary we roll a fresh set of jobs, advance the difficulty
            // cycle, and persist the generated JOB entries as `quests` rows so the
            // follow-up /objectives + /complete routes can resolve their questIds.
            // Within a window the same jobs return (deterministic seed), so a
            // re-fetch is idempotent.
            let reset_boundary = jobs_gen::current_reset_boundary(&job_pools_def, now);
            let needs_regen = character.character.0.last_jobs_reset_time < reset_boundary;

            let completed_job_ids = if needs_regen {
                std::collections::HashSet::new()
            } else {
                use crate::schema::quests;
                quests::table
                    .filter(quests::character_id.eq(character_id_var))
                    .select(QuestDbEntry::as_select())
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .filter(|q| jobs_gen::is_job_row(&q.info.0) && q.info.0.completed)
                    .map(|q| q.id)
                    .collect()
            };

            // Advance the difficulty cycle BEFORE rolling (tracker #313): every later
            // fetch in this window — and /accept and /complete — rolls from the
            // stored index, so the first roll must use that same value or the
            // board's difficulties would shift on the second fetch.
            if needs_regen {
                character.character.0.job_difficulty_cycle_index = jobs_gen::next_cycle_index(
                    &job_pools_def,
                    character.character.0.job_difficulty_cycle_index,
                );
            }
            let (jobs, job_pools) = jobs_gen::generate_replenished(
                &job_pools_def,
                character_id_var,
                character.character.0.level,
                character.character.0.job_difficulty_cycle_index,
                reset_boundary,
                now,
                &completed_job_ids,
            );

            if needs_regen {
                // Drop the previous window's JOB rows (marked by the sentinel
                // gldQuestId) so stale, un-accepted jobs don't linger, then insert
                // the freshly rolled ones. Jobs already carried into a dungeon keep a
                // real dungeon_state; we only prune the untouched catalog rows.
                let job_quest_ids: Vec<Uuid> = jobs
                    .iter()
                    .filter_map(|j| j.get("questId").and_then(|v| v.as_str()))
                    .filter_map(|s| Uuid::parse_str(s).ok())
                    .collect();
                {
                    use crate::schema::quests;
                    // Delete prior-window job rows for this character that are NOT part
                    // of the new set and have not been entered (no dungeon_state).
                    // Whatever goes here is reported to the client as `deletedQuestIds`
                    // — otherwise the board keeps showing entries we just removed.
                    let stale: Vec<Uuid> = quests::table
                        .filter(quests::character_id.eq(character_id_var))
                        .filter(quests::dungeon_state.is_null())
                        .select(QuestDbEntry::as_select())
                        .load(&mut conn)
                        .await?
                        .into_iter()
                        .filter(|q| jobs_gen::is_job_row(&q.info.0))
                        .map(|q| q.id)
                        .filter(|id| !job_quest_ids.contains(id))
                        .collect();
                    if !stale.is_empty() {
                        diesel::delete(
                            quests::table
                                .filter(quests::character_id.eq(character_id_var))
                                .filter(quests::id.eq_any(&stale)),
                        )
                        .execute(&mut conn)
                        .await?;
                        deleted_quest_ids.extend(stale.iter().copied());
                    }
                }
                // Persist the rotation scalars on the character (the cycle index
                // was advanced above, before the roll).
                character.character.0.last_jobs_reset_time = reset_boundary;
                {
                    use crate::schema::characters;
                    diesel::update(characters::table)
                        .filter(characters::id.eq(character_id_var))
                        .set(&character)
                        .execute(&mut conn)
                        .await?;
                }
            }

            // Upsert EVERY job the board is about to show — not only on a window reset.
            // A replacement job minted by `generate_replenished` between resets (after a
            // job was completed) is on the board too, and the client enters it by id:
            // with no row behind it, `…/dungeons/current/enter` 404'd and the quest never
            // started (tracker #279, 2026-09-29 12:57). Idempotent: `do_nothing` on an
            // existing row, so an entered job's dungeon_state is never touched.
            for job in &jobs {
                if let Some(entry) = jobs_gen::job_quest_db_entry(
                    job,
                    character_id_var,
                    &globals.game_data,
                    &globals.static_data.quests_daily.level_scaling,
                ) {
                    use crate::schema::quests;
                    insert_into(quests::table)
                        .values(&entry)
                        .on_conflict((quests::id, quests::character_id))
                        .do_nothing()
                        .execute(&mut conn)
                        .await?;
                }
            }

            // ---- Event ("Sigil") quests: mint the instances whose window is open ----
            // One stored GAME_EVENT row per (character, event instance). Deterministic
            // id, so re-fetching within the window resolves the SAME row and the
            // player's objective progress and milestone count survive. Inserting is
            // what makes /objectives and /complete able to find the quest at all.
            let player_level = character.character.0.level as i64;
            let minted = event_quests::mint(
                &globals.static_data,
                &globals.game_data,
                character_id_var,
                player_level,
                now as i64,
            );
            let open_event_instances: std::collections::HashSet<Uuid> =
                minted.iter().map(|m| m.quest_id).collect();
            for m in &minted {
                use crate::schema::quests;
                insert_into(quests::table)
                    .values(&QuestDbEntry {
                        id: m.quest_id,
                        character_id: character_id_var,
                        info: JsonDbWrapper(m.quest.clone()),
                        generated_data: JsonDbWrapper(m.dungeon.clone()),
                        dungeon_state: None,
                    })
                    .on_conflict((quests::id, quests::character_id))
                    .do_nothing()
                    .execute(&mut conn)
                    .await?;
            }

            // Retire event rows whose window has closed and that the player never
            // entered. Without this each character accrues a dead row per event per
            // window — about 365 a year — and the client keeps being told about
            // instances that no longer exist. Same shape as the job prune above: only
            // rows with no `dungeon_state` are removed (an entered run is left alone),
            // and whatever goes is reported as `deletedQuestIds`.
            {
                use crate::schema::quests;
                let stale: Vec<Uuid> = quests::table
                    .filter(quests::character_id.eq(character_id_var))
                    .filter(quests::dungeon_state.is_null())
                    .select(QuestDbEntry::as_select())
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .filter(|q| {
                        matches!(
                            q.info.0.r#type,
                            blades_lib::user_data::QuestType::GameEvent
                        ) && !open_event_instances.contains(&q.id)
                    })
                    .map(|q| q.id)
                    .collect();
                if !stale.is_empty() {
                    diesel::delete(
                        quests::table
                            .filter(quests::character_id.eq(character_id_var))
                            .filter(quests::id.eq_any(&stale)),
                    )
                    .execute(&mut conn)
                    .await?;
                    deleted_quest_ids.extend(stale.iter().copied());
                }
            }

            // Retire ordinary quests the player has already finished.
            //
            // `/complete` sets `info.completed = true` and leaves the row in place, so
            // the quest kept coming back in `quests[]` as a live entry. Retail never
            // does that: across 719 captured `/quests` bodies, all 563 quest entries
            // carry `completed` and **every one of them is false** — a completed quest
            // leaves the list, which is what `deletedQuestIds` (177 of those 719
            // bodies, 1033 ids) is for.
            //
            // The visible failure is report #157. The client kept offering "Rescuing
            // the Townsfolk" after it was done, and re-accepting it handed back the
            // stored row with every objective already `Completed`, so the run could
            // never be finished and the main-quest chain never moved on. 8 of the 11
            // stuck rows in prod were that one quest, on 8 different characters.
            //
            // Jobs and events are pruned above on their own schedules; this is the
            // third case and deliberately has no `dungeon_state.is_null()` guard —
            // that guard protects a run still in progress, and a completed quest has
            // none. The completion count lives on `character.completedQuests`, which
            // is what gates the chain, so dropping the row loses nothing.
            {
                use crate::schema::quests;
                let finished: Vec<Uuid> = quests::table
                    .filter(quests::character_id.eq(character_id_var))
                    .select(QuestDbEntry::as_select())
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .filter(|q| is_finished_ordinary_quest(&q.info.0))
                    .map(|q| q.id)
                    .collect();
                if !finished.is_empty() {
                    diesel::delete(
                        quests::table
                            .filter(quests::character_id.eq(character_id_var))
                            .filter(quests::id.eq_any(&finished)),
                    )
                    .execute(&mut conn)
                    .await?;
                    deleted_quest_ids.extend(finished.iter().copied());
                }
            }

            // we could have done an inner join to check the get the user id, but the user has already been checked previously.
            let mut quests = {
                use crate::schema::quests::dsl::*;
                // take care! that line above import a character_id thing
                quests::table()
                    .filter(character_id.eq(&character_id_var))
                    .select(QuestDbEntry::as_select())
                    .load(&mut conn)
                    .await?
            };

            // An event instance left `completed` between runs wedges the quest map.
            //
            // The exit before 2026-09-24 22:44 paid the next tier but never reset the
            // instance, so a character that completed a sigil run and walked out kept
            // a row reading `completed: true` with tiers still to play. `/dungeons/
            // current/enter` repairs that — but the client never gets there: it builds
            // QUESTS, JOBS and EVENTS from this response and spins on the entry, with
            // nothing but 200s in the log (Flappety, 2026-09-25: 3 of 5 tiers paid,
            // row still completed). Retail sends `completed: true` on an event only
            // once every tier is done (2 of 332 captured entries, both at their last
            // tier). So: no run in progress and tiers remaining => reset it here, the
            // same way the exit does after a completed run.
            for row in &mut quests {
                if !matches!(row.info.0.r#type, blades_lib::user_data::QuestType::GameEvent)
                    || !row.info.0.completed
                {
                    continue;
                }
                let template = row.info.0.gld_quest_id;
                let run_in_progress = {
                    use crate::schema::event_dungeons::dsl as ed;
                    ed::event_dungeons
                        .filter(ed::character_id.eq(character_id_var))
                        .filter(ed::dungeon_id.eq(template))
                        .filter(ed::dungeon_state.is_not_null())
                        .count()
                        .get_result::<i64>(&mut conn)
                        .await?
                        > 0
                };
                if run_in_progress {
                    continue;
                }
                let done = crate::dungeon::event_completion_in_window(
                    &mut conn,
                    &globals.static_data,
                    character_id_var,
                    template,
                    now as i64,
                )
                .await?
                .completion_count as usize;
                let tiers = globals
                    .static_data
                    .event_quests
                    .templates
                    .get(&template)
                    .map_or(0, |t| t.milestone_count());
                if !event_row_is_stale_completed(done, tiers) {
                    continue;
                }
                crate::dungeon::reset_event_instance_for_next_run(&mut row.info.0);
                use crate::schema::quests;
                diesel::update(
                    quests::table
                        .filter(quests::id.eq(row.id))
                        .filter(quests::character_id.eq(character_id_var)),
                )
                .set(quests::info.eq(JsonDbWrapper(row.info.0.clone())))
                .execute(&mut conn)
                .await?;
                log::info!(
                    "quests: reset stale completed event instance {} of {} for character {} \
                     ({}/{} tiers done)",
                    row.id,
                    template,
                    character_id_var,
                    done,
                    tiers
                );
            }

            // Rows accepted before capture-derived interactable loot shipped are
            // durable, so deploying the generator did not help those players. Repair
            // the stale item map on the ordinary /quests refresh that precedes play,
            // and persist it because /dungeons/current/update reads the DB row again.
            for row in &mut quests {
                if jobs_gen::is_job_row(&row.info.0) {
                    continue;
                }
                // Event rows take the enemy-key repair (#236) and the missing-stage
                // repair (#323) below: the other story repairs were written for, and
                // tested against, story rows.
                let is_event =
                    matches!(row.info.0.r#type, blades_lib::user_data::QuestType::GameEvent);
                // Repair a story quest stamped with a job's difficulty.
                //
                // A retired code path minted story quests with a real
                // difficulty level (and the tell-tale `seed: 1234`) where retail
                // sends -1 on every one of 611 captured entries. Those rows are
                // durable, so deploying the corrected accept path did nothing for
                // the players already holding one: 45 rows across 34 characters.
                //
                // The symptom is the quest map spinning forever — the client
                // builds that screen from this response and never re-requests it,
                // so one unrenderable entry wedges QUESTS, JOBS and EVENTS
                // together with nothing but 200s in the log. A character with no
                // story quest at all is unaffected, which is why this reproduced
                // on one character and not another.
                //
                // Same shape as the loot repair below, and deliberately ahead of
                // it: that one gives up when a row has no generated_data, and
                // this must run for every story row regardless.
                if !is_event && repair_story_quest_difficulty(&mut row.info.0) {
                    use crate::schema::quests;
                    diesel::update(
                        quests::table
                            .filter(quests::id.eq(row.id))
                            .filter(quests::character_id.eq(character_id_var)),
                    )
                    .set(quests::info.eq(JsonDbWrapper(row.info.0.clone())))
                    .execute(&mut conn)
                    .await?;
                }

                let Some(stored) = row.generated_data.0.as_mut() else {
                    continue;
                };
                let Ok((_, Some(fresh))) = generate_quest_data(
                    &globals.game_data,
                    row.info.0.gld_quest_id,
                    player_level,
                    &globals.static_data.quests_daily.level_scaling,
                ) else {
                    continue;
                };
                // An event row first loses any stage its event never had (#329), so
                // the repairs below cannot touch what is about to go.
                let pruned = is_event
                    && drop_foreign_event_stages(&globals.game_data, &row.info.0, stored);
                if pruned {
                    log::info!(
                        "quests: took the stage event quest {} ({}) never had out of character \
                         {}'s row (#329)",
                        row.id,
                        row.info.0.gld_quest_id,
                        character_id_var
                    );
                }
                // An event row gains the stages it was minted without (#323), from
                // its own event dungeon at its own difficulty; the story repairs
                // below stay story-only.
                let event_fresh = if is_event {
                    event_row_fresh(
                        &globals.game_data,
                        &globals.static_data.quests_daily.level_scaling,
                        row.id,
                        &row.info.0,
                    )
                } else {
                    None
                };
                let expanded = match &event_fresh {
                    Some(ev) => add_missing_dungeon_sections(stored, ev),
                    None if is_event => false,
                    None => add_missing_dungeon_sections(stored, &fresh),
                };
                // Full container piles (#353, #358, #365) for every row, and the
                // event's own enemy levels for an event row.
                let piled = grow_short_item_piles(stored, event_fresh.as_ref().unwrap_or(&fresh));
                let levelled = event_fresh
                    .as_ref()
                    .is_some_and(|ev| relevel_event_enemies(stored, ev));
                if piled || levelled {
                    log::info!(
                        "quests: filled the containers ({piled}) / set the enemy levels \
                         ({levelled}) of quest {} ({}) for character {} (#365)",
                        row.id,
                        row.info.0.gld_quest_id,
                        character_id_var
                    );
                }
                let refreshed = !is_event && refresh_empty_item_loot(stored, fresh.clone());
                let grew = !is_event && add_missing_item_tables(stored, &fresh);
                let keyed = add_missing_enemy_key_loot(stored, &fresh);
                if keyed {
                    log::info!(
                        "quests: gave the key-holder of quest {} ({}) its door key for character {}",
                        row.id,
                        row.info.0.gld_quest_id,
                        character_id_var
                    );
                }
                let filled = add_missing_spawner_enemies(stored, &fresh);
                if filled {
                    log::info!(
                        "quests: filled the short spawners of quest {} ({}) for character {} (#301)",
                        row.id,
                        row.info.0.gld_quest_id,
                        character_id_var
                    );
                }
                if pruned || expanded || refreshed || grew || keyed || filled || piled || levelled {
                    use crate::schema::quests;
                    diesel::update(
                        quests::table
                            .filter(quests::id.eq(row.id))
                            .filter(quests::character_id.eq(character_id_var)),
                    )
                    .set(quests::generated_data.eq(JsonDbWrapper(Some(stored.clone()))))
                    .execute(&mut conn)
                    .await?;
                }
            }

            let (result_quests, game_event_quests, row_generated_data) = split_quest_rows(
                quests
                    .into_iter()
                    .map(|q| (q.id, q.info.0, q.generated_data.0)),
                &open_event_instances,
            );
            let result_generated_data =
                assemble_generated_data_list(
                    row_generated_data,
                    &globals.game_data,
                    &jobs,
                    &globals.static_data.quests_daily.level_scaling,
                );

            // Events opening within the next 24h, announced but not yet playable.
            let game_event_quests_in_warning = event_quests::upcoming(
                &globals.static_data,
                &globals.game_data,
                character_id_var,
                player_level,
                now as i64,
            );

            Ok(Json(GetQuestsResponse {
                deleted_quest_ids,
                quests: result_quests,
                dungeon_generated_data_list: result_generated_data,
                character: CompleteCharacterWithIdWithoutData {
                    id: character_id_var,
                    character: character.character.0,
                },
                jobs,
                game_event_quests,
                game_event_quests_finished: Vec::new(),
                game_event_quests_in_warning,
                job_pools,
            }))
        }
        .scope_boxed()
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptQuestResponse {
    #[serde(skip_serializing_if = "RewardGrant::is_empty")]
    reward: RewardGrant,
    #[serde(skip_serializing_if = "Option::is_none")]
    inventory: Option<CompleteInventoryUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<CompleteWallet>,
    #[serde(skip_serializing_if = "Option::is_none")]
    character: Option<CompleteCharacterWithIdWithoutData>,
    quest: QuestWithId,
    dungeon_generated_data: Option<DungeonGeneratedDataWithId>,
}

/// Resources retail grants when a quest is first accepted rather than when an
/// objective or the quest completes. These values are absent from the extracted
/// quest holder, so entries belong here only when a tester supplies an exact retail
/// observation. The insert result in [`accept_quest`] makes the grant idempotent.
fn acceptance_reward(quest_id: Uuid) -> RewardGrant {
    const MQ04_REBUILD_TOWN_HALL: Uuid = Uuid::from_u128(0x3b478dfa_73bb_42df_a420_05cf83d015bc);
    const LUMBER: Uuid = Uuid::from_u128(0xe7193116_d761_479b_8a20_5633737977f5);
    const COPPER: Uuid = Uuid::from_u128(0x42d91529_c88b_4c5b_815b_b55508b4e7ef);
    const LIMESTONE: Uuid = Uuid::from_u128(0xfd67bbc6_20f4_44a3_9614_28265ebb8c67);

    let mut reward = RewardGrant::default();
    if quest_id == MQ04_REBUILD_TOWN_HALL {
        reward.stackable_items.insert(LUMBER, 140);
        reward.stackable_items.insert(COPPER, 150);
        reward.stackable_items.insert(LIMESTONE, 50);
    }
    reward
}

fn map_quest_generation_error(error: GenerateQuestDataError) -> BladeApiError {
    match error {
        // A stale client can ask to accept a quest that is no longer in the
        // server's current catalog. That is a normal resource miss, not a
        // server fault; mapping it explicitly also keeps it out of the ERROR
        // log used for real 500s.
        GenerateQuestDataError::QuestNotFound(_) => {
            BladeApiError::new(StatusCode::NOT_FOUND, 20001, 1)
        }
        // A known quest pointing at a missing dungeon is inconsistent server
        // data. Preserve the generic 500 and diagnostic log for that case.
        other => BladeApiError::generic_internal_error(other),
    }
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/accept"
)]
async fn accept_quest(
    session: SessionLookedUpMaybe,
    request: Json<Option<()>>,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<Json<AcceptQuestResponse>, BladeApiError> {
    assert!(request.is_none());
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, quest_id) = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    // A town-job questId is not present in `game_data.quests`; it is rolled from
    // the job pools. In prod the client uses jobs straight from the /quests board
    // (no /accept), but we still accept a job gracefully: regenerate the current
    // window and, if the questId is one of ours, persist it (or reuse the row the
    // /quests board already stored). Falls through to the normal quest path below
    // for regular quest ids.
    if !app_state.game_data.quests.contains_key(&quest_id) {
        // Load the character (with permission check) for the job-difficulty cycle.
        let character = {
            use crate::schema::characters::dsl::*;
            let rows = characters
                .filter(id.eq(&character_id))
                .select(CharacterDbEntryCharacterAlone::as_select())
                .load(&mut conn)
                .await?;
            util::get_only_single_character_and_check_permission(rows, &session.session)?
        };
        let now = jobs_gen::now_epoch_secs();
        let reset_boundary = jobs_gen::current_reset_boundary(&app_state.job_pools, now);
        let (jobs, _pools) = jobs_gen::generate(
            &app_state.job_pools,
            character_id,
            character.character.0.level,
            character.character.0.job_difficulty_cycle_index,
            reset_boundary,
            now,
        );
        if let Some(job) = jobs
            .iter()
            .find(|j| j.get("questId").and_then(|v| v.as_str()) == Some(&quest_id.to_string()))
        {
            if let Some(entry) = jobs_gen::job_quest_db_entry(
                job,
                character_id,
                &app_state.game_data,
                &app_state.static_data.quests_daily.level_scaling,
            ) {
                use crate::schema::quests;
                insert_into(quests::table)
                    .values(&entry)
                    .on_conflict((quests::id, quests::character_id))
                    .do_nothing()
                    .execute(&mut conn)
                    .await?;
                return Ok(Json(AcceptQuestResponse {
                    reward: RewardGrant::default(),
                    inventory: None,
                    wallet: None,
                    character: None,
                    quest: QuestWithId {
                        quest_id,
                        quest: entry.info.0,
                    },
                    // Was hard-coded `None`: accepting a job handed the client a quest
                    // with no dungeon data, the accept-path half of report #85.
                    dungeon_generated_data: entry
                        .generated_data
                        .0
                        .map(|inner| DungeonGeneratedDataWithId { quest_id, inner }),
                }));
            }
        }
        // Not a job we know about and not a real quest → let the normal path 404.
    }

    // check permission (normal-quest path) + read the character level so the quest's
    // enemies scale to the player (fix: generate_quest_data no longer hard-codes level 1).
    let character = {
        use crate::schema::characters::dsl::*;
        let rows = characters
            .filter(id.eq(&character_id))
            .select(CharacterDbEntryCharacterAlone::as_select())
            .load(&mut conn)
            .await?;
        util::get_only_single_character_and_check_permission(rows, &session.session)?
    };
    let player_level = character.character.0.level as i64;

    // actually add quest (level-scaled; a nil-dungeon dialogue quest generates no
    // dungeon data instead of erroring).
    let (quest, dungeon_generated_data) = generate_quest_data(
        &app_state.game_data,
        quest_id,
        player_level,
        &app_state.static_data.quests_daily.level_scaling,
    )
    .map_err(map_quest_generation_error)?;
    let to_insert = QuestDbEntry {
        id: quest_id,
        character_id,
        info: JsonDbWrapper(quest),
        generated_data: JsonDbWrapper(dungeon_generated_data),
        dungeon_state: None,
    };

    // Accepting an already-accepted quest returns the STORED row rather than failing
    // or resetting its progress. The insert and first-accept reward share one
    // transaction: a retry sees `inserted == 0` and cannot double-credit resources,
    // while a failed economy write rolls the new quest row back too.
    let (stored, reward, inventory, wallet, character) = conn
        .transaction(move |mut conn| {
            async move {
                use crate::schema::quests;

                // A row left over from a PREVIOUS completion is not progress to
                // protect — it is a finished quest with every objective already
                // `Completed`. Leaving it for the `do_nothing` below to return hands
                // the client a run it can never finish: that is report #157, "I can't
                // finish that quest again". Drop it so the insert makes a fresh
                // instance. `/quests` prunes these on the next board refresh too, but
                // a client that re-accepts before then would still get the corpse.
                let replayed = quests::table
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .select(QuestDbEntry::as_select())
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .is_some_and(|q| is_finished_ordinary_quest(&q.info.0));
                if replayed {
                    diesel::delete(
                        quests::table
                            .filter(quests::id.eq(quest_id))
                            .filter(quests::character_id.eq(character_id)),
                    )
                    .execute(&mut conn)
                    .await?;
                }

                let inserted = insert_into(quests::table)
                    .values(&to_insert)
                    .on_conflict((quests::id, quests::character_id))
                    .do_nothing()
                    .execute(&mut conn)
                    .await?;

                let stored = quests::table
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .select(QuestDbEntry::as_select())
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .unwrap_or(to_insert);

                // A replay re-inserts the row, so `inserted == 1` on its own no
                // longer means "first acceptance". Paying here would let anyone farm
                // the acceptance reward by finishing a quest and accepting it again,
                // so a replay pays nothing. Whether retail re-pays a genuinely
                // repeatable quest is unmeasured; not paying cannot be exploited.
                let reward = if inserted == 1 && !replayed {
                    acceptance_reward(quest_id)
                } else {
                    RewardGrant::default()
                };
                if reward.is_empty() {
                    return Ok::<_, BladeApiError>((stored, reward, None, None, None));
                }

                let mut entry = {
                    use crate::schema::characters;
                    characters::table
                        .filter(characters::id.eq(character_id))
                        .filter(characters::user_id.eq(user_id))
                        .select(CharacterDbEntryEconomy::as_select())
                        .for_no_key_update()
                        .load(&mut conn)
                        .await?
                        .into_iter()
                        .next()
                        .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
                };
                let mut tracker = InventoryChangeTracker::default();
                apply_reward(
                    &reward,
                    &mut entry.wallet.0,
                    &mut entry.inventory.0,
                    &mut entry.character.0,
                    &mut tracker,
                );
                entry.inventory.0.backpack_version += 1;
                let inventory = entry.inventory.0.generate_client_update(&tracker);
                let wallet = (!reward.currencies.is_empty()).then(|| entry.wallet.0.clone());
                let character = CompleteCharacterWithIdWithoutData {
                    id: character_id,
                    character: entry.character.0.clone(),
                };
                {
                    use crate::schema::characters;
                    diesel::update(characters::table)
                        .filter(characters::id.eq(entry.id))
                        .set(entry)
                        .execute(&mut conn)
                        .await?;
                }
                Ok((stored, reward, Some(inventory), wallet, Some(character)))
            }
            .scope_boxed()
        })
        .await?;

    Ok(Json(AcceptQuestResponse {
        reward,
        inventory,
        wallet,
        character,
        quest: QuestWithId {
            quest_id,
            quest: stored.info.0,
        },
        dungeon_generated_data: stored
            .generated_data
            .0
            .map(|inner| DungeonGeneratedDataWithId { quest_id, inner }),
    }))
}

#[cfg(test)]
mod accept_error_mapping_tests {
    use super::*;
    use actix_web::ResponseError;

    #[test]
    fn an_unknown_quest_is_a_404_not_a_generic_500() {
        let err = map_quest_generation_error(GenerateQuestDataError::QuestNotFound(Uuid::nil()));
        assert_eq!(err.status_code(), StatusCode::NOT_FOUND);
        assert_eq!(err.error_code(), 1);
    }
}

#[cfg(test)]
mod report99_quest_reward_tests {
    use super::*;

    fn game_data() -> blades_lib::game_data::GameData {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static/parsed.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn repair_data() -> RepairData {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let durability: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("item_durability.json")).unwrap(),
        )
        .unwrap();
        RepairData::from_json(&durability, &json!({}))
    }

    #[test]
    fn mq04_acceptance_grants_the_observed_resources() {
        let reward =
            acceptance_reward(Uuid::parse_str("3b478dfa-73bb-42df-a420-05cf83d015bc").unwrap());
        let lumber = Uuid::parse_str("e7193116-d761-479b-8a20-5633737977f5").unwrap();
        let copper = Uuid::parse_str("42d91529-c88b-4c5b-815b-b55508b4e7ef").unwrap();
        let limestone = Uuid::parse_str("fd67bbc6-20f4-44a3-9614-28265ebb8c67").unwrap();
        assert_eq!(reward.stackable_items.get(&lumber), Some(&140));
        assert_eq!(reward.stackable_items.get(&copper), Some(&150));
        assert_eq!(reward.stackable_items.get(&limestone), Some(&50));
        assert_eq!(reward.stackable_items.len(), 3);
    }

    #[test]
    fn other_acceptances_do_not_invent_rewards() {
        assert!(acceptance_reward(Uuid::nil()).is_empty());
    }

    #[test]
    fn lumber_run_objective_mints_the_apk_iron_war_axe() {
        let reward = objective_reward(
            &game_data(),
            &repair_data(),
            Uuid::parse_str("e53a430b-35f4-4848-8b4e-c1f0089e098a").unwrap(),
            &[Uuid::parse_str("3de9d4ab-77ac-48c6-bfac-fb4460aa2346").unwrap()],
        );
        assert_eq!(reward.items.len(), 1);
        assert_eq!(
            reward.items[0].item.item_template_id,
            Uuid::parse_str("6fe63664-4252-4269-9f51-8546b5f138da").unwrap()
        );
        assert_eq!(reward.items[0].item.tempering_level, 0);
        assert_eq!(reward.items[0].item.durability, 75.0);
        assert!(reward.stackable_items.is_empty());
    }

    #[test]
    fn restoration_potions_remain_a_stackable_objective_reward() {
        let reward = objective_reward(
            &game_data(),
            &repair_data(),
            Uuid::parse_str("5ad30483-8994-484e-b6dc-a5e9014cc4d5").unwrap(),
            &[Uuid::parse_str("76b97069-67e9-4202-aa93-8bc1dc7fbc65").unwrap()],
        );
        let potion = Uuid::parse_str("d5ccf370-0795-4554-9dab-68ccbb97473d").unwrap();
        assert_eq!(reward.stackable_items.get(&potion), Some(&5));
        assert!(reward.items.is_empty());
    }
}

/// What completing an EVENT quest pays on its `completion`-th completion, or `None`
/// once the instance is exhausted.
///
/// An event quest is repeatable and pays a MILESTONE: the Nth completion pays
/// `rewards[N]`, and the last one additionally pays `finalReward` — a bonus ON the
/// final tier, not a tier of its own. Measured across 93 retail instances — 91/93
/// first completions, 67/68 second, 59/60 third, 56/57 fourth, and all 54 observed
/// fifth completions paid the last tier merged with `finalReward`. Past the last
/// milestone there is nothing left to pay; paying `finalReward` by itself on every
/// later run made the event farmable without limit (tracker #98).
///
/// The count comes from the caller — `event_completions`, read through
/// `dungeon::event_completion_in_window` — so this stays a pure function.
pub(crate) fn event_milestone_reward(
    static_data: &blades_lib::static_data::StaticData,
    quest_id: Uuid,
    quest: &blades_lib::user_data::Quest,
    completion: usize,
) -> Option<RewardGrant> {
    let Some(tmpl) = static_data.event_quests.templates.get(&quest.gld_quest_id) else {
        log::warn!(
            "[quest] event quest {quest_id} (template {}) has no entry in \
             event_quests.json — paying nothing",
            quest.gld_quest_id
        );
        return None;
    };
    // The instance's OWN ladder first (report #333): retail minted each instance with
    // the rewards of the character's level band and paid exactly that, so the row
    // carries it — and it is what the client has been showing the player. Only a row
    // without one falls back to the template.
    let (mut reward, final_reward, milestones) =
        match quest.rewards.as_deref().filter(|r| !r.is_empty()) {
            Some(ladder) => (
                ladder.get(completion)?.clone(),
                quest.final_reward.as_ref(),
                ladder.len(),
            ),
            None => (
                tmpl.payout(completion)?,
                tmpl.final_reward.as_ref(),
                tmpl.milestone_count(),
            ),
        };
    if completion + 1 == milestones {
        if let Some(final_reward) = final_reward {
            if !reward_already_carries(&reward, final_reward) {
                merge_reward(&mut reward, final_reward);
            }
        }
    }
    grant_currencies_to_the_wallet(&mut reward);
    Some(reward)
}

/// Whether `reward` already holds every id of `extra` at least `extra`'s amount,
/// counting an id whether it sits under `currencies` or `stackableItems`.
///
/// `payableRewards` is the `/complete` BODY retail sent, and for the last tier that
/// body already includes `finalReward`: 13/13 captured final completions whose
/// template ships the same `finalReward` (report #324). Merging it again paid the
/// bonus twice. A template whose captured last tier is from another season lacks
/// it, and still gets it added — the documented model is `rewards[last] +
/// finalReward`.
fn reward_already_carries(reward: &RewardGrant, extra: &RewardGrant) -> bool {
    let amount = |id: &Uuid| {
        reward.currencies.get(id).copied().unwrap_or(0)
            + reward.stackable_items.get(id).copied().unwrap_or(0)
    };
    extra
        .currencies
        .iter()
        .chain(extra.stackable_items.iter())
        .all(|(id, n)| amount(id) >= *n)
}

/// Move gold, sigils and gems out of `stackableItems` into `currencies`.
///
/// `rewards[]` and `finalReward` list currencies as stackable items — that is the
/// DISPLAY form the client draws its milestone ladder from. Retail's `/complete`
/// grants them under `currencies`: 0 currency ids under `stackableItems` in 155
/// captured event completions. Granting one as a stackable put a "gems" item in the
/// backpack and left the final-tier rewards screen hanging (report #324: every
/// final completion of an event whose `finalReward` is gold or gems, 5/5 on prod).
fn grant_currencies_to_the_wallet(reward: &mut RewardGrant) {
    let ids: Vec<Uuid> = reward
        .stackable_items
        .keys()
        .copied()
        .filter(|id| blades_lib::economy::is_currency(*id))
        .collect();
    for id in ids {
        if let Some(n) = reward.stackable_items.remove(&id) {
            *reward.currencies.entry(id).or_insert(0) += n;
        }
    }
}

/// What completing ordinary quest `quest_id` pays.
///
/// An ordinary quest pays a fixed amount from `quest_rewards.json`. That table is
/// keyed by the TEMPLATE id, so the lookup goes through `gldQuestId` first and only
/// falls back to the row id. It used to be the other way round, against a table
/// keyed by whatever id happened to be in the captured URL — which for an event quest
/// is a per-character instance, so 78 of its 148 keys belonged to instances that will
/// never exist again and every event quest paid nothing. Event quests are paid by
/// [`event_milestone_reward`] instead.
///
/// A quest with no captured reward pays an empty grant and is logged. No number is
/// synthesised for it: observed `characterXp` spreads over 200–900 with no rule that
/// predicts it from level, category or objective count, so a constant would be a
/// fabrication wearing a fallback's clothes. `quest_rewards.json._meta` lists exactly
/// which quests are in that state.
fn resolve_completion_reward(
    static_data: &blades_lib::static_data::StaticData,
    quest_id: Uuid,
    quest: &blades_lib::user_data::Quest,
) -> RewardGrant {
    // A town job pays what its own jobSetup declared. Its sentinel gldQuestId is in
    // neither reward table, so before this branch existed every job completion fell
    // through to "paying nothing" below — a full retail reward silently dropped on
    // every job any player has ever finished here.
    if quest.gld_quest_id == jobs_gen::JOB_SENTINEL_GLD {
        if let Some(reward) = quest.job_reward.as_ref() {
            return reward.clone();
        }
        // Rows rolled before `job_reward` existed carry None. The board is re-rolled
        // at every daily reset, so this heals itself within a day; until it does,
        // pay the XP the difficulty implies rather than nothing. No gold: the count
        // was a per-job roll and is not recoverable from the row. The XP is: retail
        // fixes it by difficulty (#337). This paid ONE enemy's kill XP before.
        let xp = jobs_gen::job_reward_xp(quest.difficulty_level);
        log::info!(
            "[quest] job {quest_id} predates jobSetup reward capture — paying {xp} xp, no gold"
        );
        return RewardGrant {
            character_xp: xp,
            ..Default::default()
        };
    }

    // Template first: `quest_rewards.json` is keyed by gldQuestId.
    if let Some(r) = static_data.quest_rewards.get(&quest.gld_quest_id) {
        return r.clone();
    }
    if let Some(r) = static_data.quest_rewards.get(&quest_id) {
        return r.clone();
    }
    log::warn!(
        "[quest] no captured reward for quest {quest_id} (template {}) — paying nothing",
        quest.gld_quest_id
    );
    RewardGrant::default()
}

/// Add `extra` into `into`. Used for the last event milestone, which retail paid as
/// the tier and the `finalReward` in a single `/complete` body.
fn merge_reward(into: &mut RewardGrant, extra: &RewardGrant) {
    for (id, n) in &extra.currencies {
        *into.currencies.entry(*id).or_insert(0) += *n;
    }
    for (id, n) in &extra.stackable_items {
        *into.stackable_items.entry(*id).or_insert(0) += *n;
    }
    into.items.extend(extra.items.iter().cloned());
    into.chests.extend(extra.chests.iter().cloned());
    into.character_xp += extra.character_xp;
    into.town_xp += extra.town_xp;
}

// ---------------------------------------------------------------------------
// POST /quests/{quest_id}/complete
// ---------------------------------------------------------------------------

/// Wire shape matched from captured `/quests/{id}/complete` responses:
/// ```json
/// { "reward":{...}, "inventory":{...}, "wallet":[...], "character":{...} }
/// ```
/// `reward` is lenient: unknown quest → empty reward (all zeros / empty maps).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompleteQuestResponse {
    reward: RewardGrant,
    inventory: CompleteInventoryUpdate,
    wallet: CompleteWallet,
    character: CompleteCharacterWithIdWithoutData,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    quests: Vec<QuestWithId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    jobs: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dungeon_generated_data_list: Vec<DungeonGeneratedDataWithId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deleted_quest_ids: Vec<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_pools: Option<Value>,
}

/// Mark one stored quest completion exactly once.
///
/// `/complete` is retried by the client whenever it commits but the response is lost,
/// and report #157 left finished rows sitting in the table for a whole board refresh —
/// one prod character reached a completion count of 4 on a quest the APK permits once.
/// A row that is already `completed` is therefore an idempotency record, not permission
/// to pay and count the quest a second time.
///
/// An event instance is completed once per RUN: the exit that ends a completed run
/// sets it back to `false` (`dungeon::event_dungeon_exit`), as retail's does.
/// Whether an event instance marked `completed` is a leftover between runs rather
/// than a finished event: tiers remain. An event with unknown tiers (0) is left as is.
fn event_row_is_stale_completed(tiers_done: usize, tiers: usize) -> bool {
    tiers > 0 && tiers_done < tiers
}

#[cfg(test)]
mod stale_completed_event_row {
    use super::event_row_is_stale_completed;

    #[test]
    fn a_completed_row_with_tiers_left_is_stale() {
        // Flappety, 2026-09-25: 3 of 5 paid, row still completed -> quest map spun.
        assert!(event_row_is_stale_completed(3, 5));
        assert!(event_row_is_stale_completed(0, 5));
    }

    #[test]
    fn a_fully_finished_event_keeps_its_completed_flag() {
        // Retail's only `completed: true` event entries are at their last tier.
        assert!(!event_row_is_stale_completed(5, 5));
        assert!(!event_row_is_stale_completed(6, 5));
        assert!(
            !event_row_is_stale_completed(0, 0),
            "unknown template: leave it"
        );
    }
}

fn mark_quest_completed_once(info: &mut blades_lib::user_data::Quest) -> bool {
    if info.completed {
        return false;
    }
    info.completed = true;
    true
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/complete"
)]
pub async fn complete_quest(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<Json<CompleteQuestResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, quest_id) = path.into_inner();
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();
    let now = chrono::Utc::now().timestamp();

    conn.transaction(move |conn| {
        async move {
            complete_quest_in_tx(
                conn,
                &globals.static_data,
                &globals.game_data,
                &globals.job_pools,
                user_id,
                character_id,
                quest_id,
                now,
            )
            .await
        }
        .scope_boxed()
    })
    .await
    .map(Json)
}

/// The body of [`complete_quest`], inside its transaction.
///
/// **Event milestones are paid here, and only here.** Retail pays them on
/// `/complete` — 217/217 captured responses carry `reward` — and never on the
/// dungeon exit, 0/216. This server paid them on EXIT, so dying paid, walking in
/// and out paid all five tiers without playing, and a first completion paid twice:
/// once here from a second counter in `server_state`, once more on the way out.
/// Now there is one counter, `event_completions`, advanced once per completed run.
pub(crate) async fn complete_quest_in_tx(
    conn: &mut diesel_async::AsyncPgConnection,
    static_data: &blades_lib::static_data::StaticData,
    game_data: &blades_lib::game_data::GameData,
    job_pools_def: &Value,
    user_id: Uuid,
    character_id: Uuid,
    quest_id: Uuid,
    now: i64,
) -> Result<CompleteQuestResponse, BladeApiError> {
    // Load the economy row (character + wallet + inventory) under a row lock.
    let mut entry = {
        use crate::schema::characters;
        characters::table
            .filter(characters::id.eq(character_id))
            .filter(characters::user_id.eq(user_id))
            .select(CharacterDbEntryEconomy::as_select())
            .for_no_key_update()
            .load(conn)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
    };

    // Load the quest row. Its completed flag is also our idempotency record.
    let mut quest_entry = {
        use crate::schema::quests;
        quests::table
            .filter(quests::id.eq(quest_id))
            .filter(quests::character_id.eq(character_id))
            .select(QuestDbEntry::as_select())
            .for_no_key_update()
            .load(conn)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20001, 1))?
    };

    // A replayed `/complete` answers with the same shape and an empty reward:
    // the row already holds the completion, so nothing is paid or counted twice.
    if !mark_quest_completed_once(&mut quest_entry.info.0) {
        let tracker = InventoryChangeTracker::default();
        return Ok(CompleteQuestResponse {
            reward: RewardGrant::default(),
            inventory: entry.inventory.0.generate_client_update(&tracker),
            wallet: entry.wallet.0.clone(),
            character: CompleteCharacterWithIdWithoutData {
                id: character_id,
                character: entry.character.0.clone(),
            },
            quests: Vec::new(),
            jobs: Vec::new(),
            dungeon_generated_data_list: Vec::new(),
            deleted_quest_ids: Vec::new(),
            job_pools: None,
        });
    }

    if !entry.character.0.completed_quests.is_object() {
        entry.character.0.completed_quests = json!({});
    }
    // The key is the quest's OWN id for an event, and its template id otherwise —
    // see `completed_quests_key` for the measurement (report #166).
    let key = completed_quests_key(&quest_entry.info.0, quest_id);

    let is_event = matches!(
        quest_entry.info.0.r#type,
        blades_lib::user_data::QuestType::GameEvent
    );
    let (reward, completed_count) = if is_event {
        let template_id = quest_entry.info.0.gld_quest_id;
        let mut completion = crate::dungeon::event_completion_in_window(
            conn,
            static_data,
            character_id,
            template_id,
            now,
        )
        .await?;
        let tier = completion.completion_count as usize;
        let milestones = static_data
            .event_quests
            .templates
            .get(&template_id)
            .map_or(0, |t| t.milestone_count());
        match event_milestone_reward(static_data, quest_id, &quest_entry.info.0, tier) {
            Some(reward) => {
                completion.increment_completion(conn).await?;
                // Report #183: "events upon completion give negative value of
                // sigils". The completion table stores only a count, so there was no
                // way to see what a completion actually paid, and the container's log
                // is discarded every time the image is replaced. This records the
                // payout at the moment it is granted, so the next report is
                // answerable from the log instead of from a request to the reporter.
                // Currency ids are logged raw: resolving them to names would need
                // the item table in a hot path for no gain.
                log::info!(
                    "event complete: character {} tier {}/{} of event quest {} pays xp={} \
                     townXp={} currencies={:?}",
                    character_id,
                    tier + 1,
                    milestones,
                    template_id,
                    reward.character_xp,
                    reward.town_xp,
                    reward.currencies,
                );
                (reward, completion.completion_count as u64)
            }
            None => {
                log::info!(
                    "event complete: character {} has completed all {} tiers of event quest \
                     {} - no reward",
                    character_id,
                    milestones,
                    template_id
                );
                (RewardGrant::default(), completion.completion_count as u64)
            }
        }
    } else {
        let current_count = entry.character.0.completed_quests[&key]
            .as_u64()
            .unwrap_or(0);
        (
            resolve_completion_reward(static_data, quest_id, &quest_entry.info.0),
            current_count + 1,
        )
    };

    // Mirror the completion into `completedQuests` — what the client's tier
    // checkmarks read. For an event it is the tier counter itself, so the two cannot
    // drift: 1-5 over 185 retail samples, every one of those rows carrying 5 tiers.
    entry
        .character
        .0
        .completed_quests
        .as_object_mut()
        .unwrap()
        .insert(key, json!(completed_count));

    let mut tracker = InventoryChangeTracker::default();
    apply_reward(
        &reward,
        &mut entry.wallet.0,
        &mut entry.inventory.0,
        &mut entry.character.0,
        &mut tracker,
    );
    if !reward.stackable_items.is_empty() || !reward.items.is_empty() {
        entry.inventory.0.backpack_version += 1;
    }
    if !reward.chests.is_empty() {
        for chest in &reward.chests {
            grant_chest(
                &mut entry.inventory.0,
                chest.tier,
                chest.level,
                &mut tracker,
            );
        }
        entry.inventory.0.treasury_version += 1;
    }

    let inventory = entry.inventory.0.generate_client_update(&tracker);
    let wallet = entry.wallet.0.clone();
    let character = entry.character.0.clone();
    let is_job = jobs_gen::is_job_row(&quest_entry.info.0);
    let mut response_quests = Vec::new();
    let mut response_jobs = Vec::new();
    let mut response_generated_data = Vec::new();
    let mut response_deleted_quest_ids = Vec::new();
    let mut response_job_pools = None;

    if is_job {
        let reset_boundary = jobs_gen::current_reset_boundary(job_pools_def, now.max(0) as u64);
        let mut completed_job_ids = {
            use crate::schema::quests;
            quests::table
                .filter(quests::character_id.eq(character_id))
                .select(QuestDbEntry::as_select())
                .load(conn)
                .await?
                .into_iter()
                .filter(|q| jobs_gen::is_job_row(&q.info.0) && q.info.0.completed)
                .map(|q| q.id)
                .collect::<std::collections::HashSet<_>>()
        };
        completed_job_ids.insert(quest_id);

        let existing_live_job_ids = {
            use crate::schema::quests;
            quests::table
                .filter(quests::character_id.eq(character_id))
                .select(QuestDbEntry::as_select())
                .load(conn)
                .await?
                .into_iter()
                .filter(|q| {
                    q.id != quest_id && jobs_gen::is_job_row(&q.info.0) && !q.info.0.completed
                })
                .map(|q| q.id)
                .collect::<std::collections::HashSet<_>>()
        };

        let (jobs, pools) = jobs_gen::generate_replenished(
            job_pools_def,
            character_id,
            character.level,
            character.job_difficulty_cycle_index,
            reset_boundary,
            now.max(0) as u64,
            &completed_job_ids,
        );
        for job in &jobs {
            let Some(row) = jobs_gen::job_quest_db_entry(
                job,
                character_id,
                game_data,
                &static_data.quests_daily.level_scaling,
            ) else {
                continue;
            };
            let is_new = !existing_live_job_ids.contains(&row.id);
            {
                use crate::schema::quests;
                insert_into(quests::table)
                    .values(&row)
                    .on_conflict((quests::id, quests::character_id))
                    .do_nothing()
                    .execute(conn)
                    .await?;
            }
            if is_new {
                if let Some(inner) = row.generated_data.0 {
                    response_generated_data.push(DungeonGeneratedDataWithId {
                        quest_id: row.id,
                        inner,
                    });
                }
            }
        }

        response_quests = {
            use crate::schema::quests;
            quests::table
                .filter(quests::character_id.eq(character_id))
                .select(QuestDbEntry::as_select())
                .load(conn)
                .await?
                .into_iter()
                .filter(|q| {
                    !jobs_gen::is_job_row(&q.info.0)
                        && !q.info.0.completed
                        && !matches!(q.info.0.r#type, blades_lib::user_data::QuestType::GameEvent)
                        && q.generated_data.0.is_some()
                })
                .map(|q| QuestWithId {
                    quest_id: q.id,
                    quest: q.info.0,
                })
                .collect()
        };
        response_deleted_quest_ids.push(quest_id);
        response_jobs = jobs;
        response_job_pools = Some(pools);
    }

    // Write the completed quest flag back.
    {
        use crate::schema::quests;
        diesel::update(quests::table)
            .filter(quests::id.eq(quest_id))
            .filter(quests::character_id.eq(character_id))
            .set(QuestDbEntryInfo {
                info: quest_entry.info,
            })
            .execute(conn)
            .await?;
    }

    // Write the economy (wallet + inventory + character XP) back.
    {
        use crate::schema::characters;
        diesel::update(characters::table)
            .filter(characters::id.eq(entry.id))
            .set(entry)
            .execute(conn)
            .await?;
    }

    Ok(CompleteQuestResponse {
        reward,
        inventory,
        wallet,
        character: CompleteCharacterWithIdWithoutData {
            id: character_id,
            character,
        },
        quests: response_quests,
        jobs: response_jobs,
        dungeon_generated_data_list: response_generated_data,
        deleted_quest_ids: response_deleted_quest_ids,
        job_pools: response_job_pools,
    })
}

// ---------------------------------------------------------------------------
// POST /quests/{quest_id}/objectives
// ---------------------------------------------------------------------------

/// `objectiveUpdates` maps objective UUID → `{status, progress}` (and optionally
/// `completed`). The client reports absolute progress; we merge it in and persist.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ObjectivesRequest {
    #[serde(default)]
    objective_updates: std::collections::HashMap<Uuid, ObjectiveUpdate>,
}

/// One objective's absolute progress as the client reports it.
///
/// The client sends exactly `{status, progress}` — 1409 of 1409 captured
/// `objectiveUpdates` entries have those two keys and nothing else. In particular it
/// never sends `completed`, so completion has to be read off `status == Completed`.
/// Reading a `completed` flag that never arrives left `any_newly_completed` false on
/// every request, which silently disabled the objective-reward path entirely.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct ObjectiveUpdate {
    status: blades_lib::user_data::QuestStatus,
    progress: f64,
}

/// Wire shape for the objectives response.
///
/// The retail corpus has exactly three shapes, and the key the quest comes back
/// under is part of the contract:
///
/// | shape | n | when |
/// |---|---|---|
/// | `{quest}` | 856 | ordinary quest, no objective reward |
/// | `{gameEventQuest}` | 363 | event quest — **always** just the quest |
/// | `{character, inventory, quest, reward}` | 42 | ordinary quest whose objective carries a reward |
///
/// Two things follow. An event quest never pays here (its milestones are paid at
/// `/complete`), and an ordinary quest pays only what that OBJECTIVE is worth in
/// `parsed.json` — not the whole quest reward, which is what this handler used to
/// grant and which would have double-paid against `/complete`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ObjectivesResponse {
    #[serde(skip_serializing_if = "RewardGrant::is_empty")]
    reward: RewardGrant,
    #[serde(skip_serializing_if = "Option::is_none")]
    inventory: Option<CompleteInventoryUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    character: Option<CompleteCharacterWithIdWithoutData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quest: Option<QuestWithId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    game_event_quest: Option<QuestWithId>,
}

/// Which of the two response slots the updated quest goes into.
///
/// Retail answers an event quest under `gameEventQuest` (363 responses) and an
/// ordinary one under `quest` (856, plus 42 that also carry a reward). The client
/// reads the two keys differently — one drives the milestone track, the other the
/// quest log — so a quest in the wrong slot is silently ignored.
fn objectives_wire_slot(
    is_event: bool,
    quest: QuestWithId,
) -> (Option<QuestWithId>, Option<QuestWithId>) {
    if is_event {
        (None, Some(quest))
    } else {
        (Some(quest), None)
    }
}

/// What newly completing `objective_ids` on `gld_quest_id` is worth.
///
/// Straight from `parsed.json`: each objective carries a `rewards[]` list with
/// `experience` and `town_points`. 20 of the 332 shipped objectives have one, which
/// is the population behind retail's 42 reward-bearing `/objectives` responses.
///
/// `items_to_reward` comes from the same APK objective definitions. A template with
/// APK repair data is breakable gear and is therefore minted as an instanced item;
/// currencies credit the wallet, and the remaining materials, potions and quest
/// objects are stackables. This distinction covers all 18 non-zero item rewards in
/// the shipped corpus without guessing from localized names or client input.
fn objective_reward(
    game_data: &blades_lib::game_data::GameData,
    repair_data: &RepairData,
    gld_quest_id: Uuid,
    objective_ids: &[Uuid],
) -> RewardGrant {
    let mut out = RewardGrant::default();
    let Some(info) = game_data
        .quests
        .get(&gld_quest_id)
        .and_then(|q| q.dungeon_info.as_ref())
    else {
        return out;
    };
    for oid in objective_ids {
        let Some(objective) = info.objectives.get(oid) else {
            continue;
        };
        for r in &objective.rewards {
            out.character_xp += r.experience.max(0.0) as u64;
            out.town_xp += r.town_points;
            for item in &r.items_to_reward {
                if item.count == 0 {
                    continue;
                }
                if is_currency(item.template_uuid) {
                    *out.currencies.entry(item.template_uuid).or_insert(0) += item.count;
                } else if let Some(durability) = repair_data.max_durability(item.template_uuid, 0) {
                    for _ in 0..item.count {
                        out.items.push(RewardItem {
                            id: Uuid::new_v4(),
                            item: Item {
                                item_template_id: item.template_uuid,
                                grade: None,
                                tempering_level: 0,
                                durability,
                                properties: ItemPropertiesAll::default(),
                                arcane_tier: None,
                            },
                        });
                    }
                } else {
                    *out.stackable_items.entry(item.template_uuid).or_insert(0) += item.count;
                }
            }
        }
    }
    out
}

#[post(
    "/blades.bgs.services/api/game/v1/public/characters/{character_id}/quests/{quest_id}/objectives"
)]
pub async fn update_quest_objectives(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<(Uuid, Uuid)>,
    body: Json<ObjectivesRequest>,
) -> Result<Json<ObjectivesResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let user_id = session.session.user_id;
    let (character_id, quest_id) = path.into_inner();
    let body = body.into_inner();
    let globals = app_state.get_ref().clone();
    let mut conn = app_state.db_pool.get().await.unwrap();

    conn.transaction(move |mut conn| {
        async move {
            // Load the economy row under a row lock (needed only when a reward is granted,
            // but we can't know upfront; take it eagerly to keep the transaction simple).
            let mut entry = {
                use crate::schema::characters;
                characters::table
                    .filter(characters::id.eq(character_id))
                    .filter(characters::user_id.eq(user_id))
                    .select(CharacterDbEntryEconomy::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20000, 2))?
            };

            // Load and lock the quest row.
            let mut quest_entry = {
                use crate::schema::quests;
                quests::table
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .select(QuestDbEntry::as_select())
                    .for_no_key_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| BladeApiError::new(StatusCode::NOT_FOUND, 20001, 1))?
            };

            // Merge each objective update in. The client sends absolute progress, and
            // reports completion as `status: "Completed"` — there is no `completed`
            // flag on the wire.
            let mut newly_completed: Vec<Uuid> = Vec::new();
            for (obj_id, update) in &body.objective_updates {
                let entry_obj = quest_entry
                    .info
                    .0
                    .objective_statuses
                    .entry(*obj_id)
                    .or_insert_with(|| blades_lib::user_data::ObjectiveStatus {
                        status: blades_lib::user_data::QuestStatus::Active,
                        progress: 0.0,
                        completed: false,
                    });
                entry_obj.status = update.status;
                entry_obj.progress = update.progress;
                let done = matches!(update.status, blades_lib::user_data::QuestStatus::Completed);
                if done && !entry_obj.completed {
                    entry_obj.completed = true;
                    newly_completed.push(*obj_id);
                }
            }

            let is_event = matches!(
                quest_entry.info.0.r#type,
                blades_lib::user_data::QuestType::GameEvent
            );
            // An event quest never pays here — all 363 captured `{gameEventQuest}`
            // responses are the quest alone, and its milestones are paid at /complete.
            let reward = if is_event || newly_completed.is_empty() {
                RewardGrant::default()
            } else {
                objective_reward(
                    &globals.game_data,
                    &globals.repair_data,
                    quest_entry.info.0.gld_quest_id,
                    &newly_completed,
                )
            };

            let (opt_inventory, opt_character) = if !reward.is_empty() {
                let mut tracker = InventoryChangeTracker::default();
                apply_reward(
                    &reward,
                    &mut entry.wallet.0,
                    &mut entry.inventory.0,
                    &mut entry.character.0,
                    &mut tracker,
                );
                if !reward.stackable_items.is_empty() || !reward.items.is_empty() {
                    entry.inventory.0.backpack_version += 1;
                }
                if !reward.chests.is_empty() {
                    for chest in &reward.chests {
                        grant_chest(
                            &mut entry.inventory.0,
                            chest.tier,
                            chest.level,
                            &mut tracker,
                        );
                    }
                    entry.inventory.0.treasury_version += 1;
                }
                let inv = entry.inventory.0.generate_client_update(&tracker);
                let ch = entry.character.0.clone();
                // Write economy back.
                {
                    use crate::schema::characters;
                    diesel::update(characters::table)
                        .filter(characters::id.eq(entry.id))
                        .set(entry)
                        .execute(&mut conn)
                        .await?;
                }
                (
                    Some(inv),
                    Some(CompleteCharacterWithIdWithoutData {
                        id: character_id,
                        character: ch,
                    }),
                )
            } else {
                (None, None)
            };

            let quest_with_id = QuestWithId {
                quest_id,
                quest: quest_entry.info.0.clone(),
            };

            // Persist the updated objective statuses.
            {
                use crate::schema::quests;
                diesel::update(quests::table)
                    .filter(quests::id.eq(quest_id))
                    .filter(quests::character_id.eq(character_id))
                    .set(QuestDbEntryInfo {
                        info: quest_entry.info,
                    })
                    .execute(&mut conn)
                    .await?;
            }

            let (quest, game_event_quest) = objectives_wire_slot(is_event, quest_with_id);
            Ok::<_, BladeApiError>(Json(ObjectivesResponse {
                reward,
                inventory: opt_inventory,
                character: opt_character,
                quest,
                game_event_quest,
            }))
        }
        .scope_boxed()
    })
    .await
}

// ===========================================================================
// Town-job generation (`jobs_gen`)
// ===========================================================================
//
// Faithful generation of the `/quests` `jobs[]` board and per-pool `jobPools`
// timers from `app_state.job_pools` (server/data/static/job_pools.json, itself
// extracted from the APK). Ground truth for the wire shape is the prod capture
// (a JOB entry = `{questId, version, type:"JOB", objectiveStatuses,
// difficultyLevel, seed, jobPoolId, jobSetup:{…}, completed}`; timers are
// `[{id, endTime, nextStartTime}]` in epoch **seconds**).
//
// Design:
//   * Generation is **deterministic** from `(character_id, reset_boundary,
//     pool_id, slot)`, so the same board returns on every /quests fetch within a
//     reset window without persisting the job bodies. `questId`s are derived the
//     same way, so /objectives + /complete can resolve an accepted job.
//   * The enemy-family / dungeon-template / gather-item / duel-boss IDs are not
//     present in job_pools.json (they live in the APK dungeon bundles), so they
//     are drawn from constant pools harvested from the captures — enough for a
//     faithful, acceptable board. Everything else (spawn groups, boss loot,
//     difficulty ranges, gem-skip, duel-boss list) comes straight from
//     job_pools.json.
//   * Never panics on the `Value` shape: a malformed / Null `job_pools` yields an
//     empty jobs list and empty timers rather than a 500.
pub(crate) mod jobs_gen {
    use super::*;
    use blades_lib::game_data::GameData;
    use blades_lib::static_data::QuestLevelScaling;
    use blades_lib::user_data::{
        DungeonGeneratedData, ObjectiveStatus, Quest, QuestStatus, QuestType,
    };
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// The dungeon whose spawn groups EVERY town job's generated data is keyed to.
    ///
    /// `parsed.json` calls it `JobSpawnGroupsReference` (6 enemy spawn groups, 7 item
    /// spawns, 2 chests). It is not a playable layout — it is the shared id space the
    /// job generator draws from, and the client resolves a job's spawn ids against it
    /// whatever `jobSetup.dungeonTemplateId` says.
    ///
    /// MEASURED, not guessed. In the one committed full `/quests` body
    /// (`blades-capture reference/capture-599.jsonl`: 6 jobs, 2 quests, 10 generated-data
    /// entries) all six job entries' enemy/item/chest ids are subsets of this dungeon's,
    /// and the two story quests in the same body — the control — are subsets of neither.
    ///
    /// The `JobCaveVariant_03`-style ids in [`DUNGEON_TEMPLATES`] are NOT usable here:
    /// all 17 of them exist in `parsed.json` with a completely EMPTY `spawn_info`, so
    /// generating from the template id yields an entry with no enemies, no items and no
    /// chests. That is what made this look like missing data rather than a wrong key.
    pub const JOB_SPAWN_GROUPS_REFERENCE: Uuid =
        Uuid::from_u128(0x93202b6a_f74e_49d4_ab70_bd46cc2f9892_u128);

    /// The three enemy groups retail emits for a Duel job: two fixed one-enemy
    /// encounter groups plus the configured boss group. In capture-599, Duel
    /// job `64312841-…` has exactly these three, no primary/secondary/secret
    /// groups, and no item-generated data. The old generator emitted all six
    /// reference groups (14 enemies) and all seven item groups for every job,
    /// which gives the Duel scene data for objects that do not exist there and
    /// leaves the client waiting at load.
    pub(super) const DUEL_ENEMY_SPAWN_GROUPS: [Uuid; 3] = [
        Uuid::from_u128(0x132fc1b2_8480_4f45_a10d_b2c708db85a7_u128),
        Uuid::from_u128(0xd7b105e7_7a76_4b92_b9ba_21a5bae4133e_u128),
        Uuid::from_u128(0x690d51c4_46c3_4546_bf82_e267289aca02_u128),
    ];
    pub(super) const JOB_BOSS_SPAWN_GROUP: Uuid =
        Uuid::from_u128(0x690d51c4_46c3_4546_bf82_e267289aca02_u128);

    // The remaining groups the reference dungeon carries, from
    // `job_pools.json` → `globals.enemySpawnGroups` and `parsed.json`'s
    // `JobSpawnGroupsReference` (where `b7c5dbab` is named "Gather").
    //
    // A NON-duel job needs the same pruning a Duel got. The comment above says the
    // old generator "gives the Duel scene data for objects that do not exist there
    // and leaves the client waiting at load" — that is true of ordinary jobs too,
    // and the duel fix only ever pruned `jobType == 5`.
    //
    // Retail's rules, over 1395 captured non-duel jobs, all three perfect bijections
    // with no counterexample:
    //
    //   secret-boss group present  <=>  jobSetup.secretBossEnemyFamilyId present
    //                                   (233/233 yes, 0/1162 no; and 0/789 of
    //                                    `secretRoom:false` jobs carry it)
    //   primary spawner count      ==   jobSetup.primaryEnemyCount   (1395/1395)
    //   secondary spawner count    ==   jobSetup.secondaryEnemyCount (1395/1395)
    //   "Gather" item group        <=>  jobSetup.gatherItemId present
    //                                   (288/288 yes, 0/1107 no)
    //
    // We sent all six enemy groups and all seven item groups on every job, with the
    // reference's own authored counts (6 primary, 4 secondary). On the live board
    // that put the secret-room boss into 219 of 564 job entries whose own setup says
    // there is no secret room — 39%, which is the "some jobs" of report #168.
    //
    // Keyed on `secretBossEnemyFamilyId`, NOT on `secretRoom`: retail has 373 jobs
    // with a secret room and no secret boss, so the two are not the same condition.
    pub(super) const JOB_PRIMARY_SPAWN_GROUP: Uuid =
        Uuid::from_u128(0xb2a7471e_7a44_47a6_b483_503a9e5cae3d_u128);
    pub(super) const JOB_SECONDARY_SPAWN_GROUP: Uuid =
        Uuid::from_u128(0xc9ad5aae_200b_48fd_860e_ad45e0349ef0_u128);
    pub(super) const JOB_SECRET_BOSS_SPAWN_GROUP: Uuid =
        Uuid::from_u128(0xdf0f7a93_5f9d_4d36_b9f9_c087da9858de_u128);
    pub(super) const JOB_GATHER_ITEM_SPAWN_GROUP: Uuid =
        Uuid::from_u128(0xb7c5dbab_14b9_4157_a3f3_b1d34e676864_u128);

    // The rarity-ranked floor-item groups, verbatim from `job_pools.json` →
    // `globals.interactableItemSpawnGroups` and `…InSecrets`.
    //
    // RULE 4, and it is authored data rather than a statistical fit. The first pass
    // of this fix pruned the ENEMY groups and left the item groups alone, so every
    // ordinary job still carried all of them. Measured over the same 1395 captured
    // non-duel jobs:
    //
    //   both rarity-1 groups present                     1395/1395
    //   EXACTLY one of the secrets r2/r3                  1395/1395
    //       (27f1e408 x932, d00e3919 x463 — never both, never neither)
    //   AT MOST one of the main r2/r3                     1395/1395
    //       (f1753fab x688, f85079bc x346, neither x361 — never both)
    //   a Duel carries none at all                         137/137
    //
    // Retail therefore ships 3-5 item groups; we shipped 6 or 7, over-sending on
    // 1395 of 1395 jobs. The control on that measurement is that the same
    // calculation over the ENEMY groups returns zero over-send after the first
    // pass, so the item result is not an artefact of the probe.
    //
    // Which of r2/r3 a job gets does not correlate with jobType, secretRoom,
    // secretBossEnemyFamilyId, gatherItemId or rescueNpcCount — every ratio sits at
    // the base rate — so it reads as a per-job rarity roll. It is reproduced here
    // deterministically from the job's own `seed` at the measured frequencies. The
    // exact group retail picked for a given job is not recoverable; what matters for
    // the client is that the SHAPE is right and nothing is sent for scenery the
    // scene does not contain.
    /// Referenced only by the tests, which assert it against `job_pools.json` —
    /// the pruning below never needs to name a group it always keeps.
    #[cfg(test)]
    pub(super) const JOB_ITEM_R1: Uuid =
        Uuid::from_u128(0x49adb60a_f5b2_4668_b96d_d69602a326cc_u128);
    pub(super) const JOB_ITEM_R2: Uuid =
        Uuid::from_u128(0xf1753fab_bd87_48a7_a2b4_79752b059c1c_u128);
    pub(super) const JOB_ITEM_R3: Uuid =
        Uuid::from_u128(0xf85079bc_4873_4773_b8fd_d78bf1a837c1_u128);
    /// Test-only, for the same reason as [`JOB_ITEM_R1`].
    #[cfg(test)]
    pub(super) const JOB_SECRET_ITEM_R1: Uuid =
        Uuid::from_u128(0xda153b3a_8a61_460c_9384_9e12ba8f50e3_u128);
    pub(super) const JOB_SECRET_ITEM_R2: Uuid =
        Uuid::from_u128(0x27f1e408_8cfd_4ded_b177_05edc3ae296c_u128);
    pub(super) const JOB_SECRET_ITEM_R3: Uuid =
        Uuid::from_u128(0xd00e3919_826d_4476_9f36_4cf5a581ab37_u128);

    /// Stored JOB `quests` rows are tagged with this sentinel `gldQuestId` so the
    /// /quests handler can (a) keep them out of the `quests[]` array and (b)
    /// recognise a prior-window job row when pruning. It is a fixed, otherwise
    /// unused UUID — no real quest carries it.
    pub const JOB_SENTINEL_GLD: Uuid = Uuid::from_u128(0x30B10B5F_0000_4A0B_8000_000000000B0B_u128);

    /// Default daily reset hour (UTC) when the pool defs don't specify one.
    const DEFAULT_RESET_HOUR: u64 = 5;
    const SECS_PER_DAY: u64 = 86_400;

    pub fn now_epoch_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    // -- deterministic PRNG (splitmix64) -----------------------------------
    // Self-contained so the module needs no extra crate. Same seed → same rolls.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        /// Uniform in `[0, n)` (n>0).
        fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next_u64() % n }
        }
        fn pick<'a, T>(&mut self, slice: &'a [T]) -> Option<&'a T> {
            if slice.is_empty() {
                None
            } else {
                Some(&slice[self.below(slice.len() as u64) as usize])
            }
        }
        fn range_incl(&mut self, lo: i64, hi: i64) -> i64 {
            if hi <= lo {
                lo
            } else {
                lo + self.below((hi - lo + 1) as u64) as i64
            }
        }
    }

    /// Hash a `(character, boundary, pool, slot)` tuple into a PRNG seed.
    fn seed_for(character_id: Uuid, reset_boundary: u64, pool_id: &str, slot: u64) -> u64 {
        let mut h: u64 = 0xCBF2_9CE4_8422_2325; // FNV-1a offset basis
        let mut mix = |bytes: &[u8]| {
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01B3);
            }
        };
        mix(character_id.as_bytes());
        mix(&reset_boundary.to_le_bytes());
        mix(pool_id.as_bytes());
        mix(&slot.to_le_bytes());
        h
    }

    /// Derive a deterministic v4-shaped UUID from a seed.
    fn uuid_from_seed(seed: u64) -> Uuid {
        let mut rng = Rng::new(seed);
        let mut bytes = [0u8; 16];
        bytes[0..8].copy_from_slice(&rng.next_u64().to_le_bytes());
        bytes[8..16].copy_from_slice(&rng.next_u64().to_le_bytes());
        // Stamp version 4 + RFC-4122 variant so it is a well-formed UUID.
        bytes[6] = (bytes[6] & 0x0F) | 0x40;
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        Uuid::from_bytes(bytes)
    }

    // -- faithful constant pools (harvested from prod captures) ------------
    // The APK dungeon bundles (not job_pools.json) hold these; the captured set
    // is representative and keeps generated jobs acceptable/displayable.
    const DUNGEON_TEMPLATES: &[&str] = &[
        "18e81559-3561-47ef-b73e-9f3bc34ba0b8",
        "19a3b1b0-c18b-4f2f-b73f-780f3759fe48",
        "3bcfeff9-5b22-4f7c-b1b8-ef4b277f7bc2",
        "4d3153a0-cfc5-405c-b065-92547ee9fbbc",
        "57d639c2-ec4c-4e6b-9995-ff6a7ef3e712",
        "5af68bb1-e478-4a2d-916c-651b8d793749",
        "62598ab9-82ac-4321-8534-a048edd26ccb",
        "6c9fe3b7-2557-408a-b52a-1842680fed3f",
        "79757d9d-8a26-4a19-bd2e-49ba8577a007",
        "86e6a720-caa4-4b13-b8b8-73f3dcf049a2",
        "a9386df1-5b26-462b-9c56-de9cb371c790",
        "b13c209c-3f41-48ac-998c-e15482e3a1a0",
        "c232c91f-37cb-4254-b8be-31f9c9d5dc54",
        "c45ba434-a2a7-4fff-99e2-8aa703f13893",
        "c5da36d7-5e28-454c-b1f5-ba57c2f5c0c4",
        "dbfd45fe-8c8c-4c8d-83c6-9b4566afc788",
        "e7418cc7-01de-4c84-ba00-e221f8783d51",
    ];
    /// The dungeon templates a Duel (the one-on-one "Champion" job) is fought in:
    /// the arena layouts, and nothing else. [report #237]
    ///
    /// Retail, over the 463 distinct jobs in the 2026-06-07 capture snapshot: all
    /// 46 Duels sat on one of these five (`JobArenaVariant_01/03/04/05/06`, 8-11
    /// each), and 0 of 417 non-duel jobs did. Drawing a Duel from
    /// [`DUNGEON_TEMPLATES`] put roughly 12 in 17 of them in a cave, forest, Ayleid
    /// ruin or stone dungeon, named after that place.
    ///
    /// `JobArenaVariant_02` (`46f8e6e8…`) exists in `parsed.json` but is left out:
    /// retail never rolled it (0/46, where a uniform pick over six would miss it
    /// with p ≈ 0.0002), and the localization table has no `UI.Arena.*.Title` for
    /// it, so a Duel there would go out with an empty name list and wedge the quest
    /// map (see [`nameable`]).
    pub(super) const DUEL_DUNGEON_TEMPLATES: &[&str] = &[
        "e7418cc7-01de-4c84-ba00-e221f8783d51", // JobArenaVariant_01
        "a9386df1-5b26-462b-9c56-de9cb371c790", // JobArenaVariant_03
        "dbfd45fe-8c8c-4c8d-83c6-9b4566afc788", // JobArenaVariant_04
        "19a3b1b0-c18b-4f2f-b73f-780f3759fe48", // JobArenaVariant_05
        "3bcfeff9-5b22-4f7c-b1b8-ef4b277f7bc2", // JobArenaVariant_06
    ];
    /// Where a job of this type may be rolled. Only a Duel is narrowed; every other
    /// type keeps drawing from the full pool exactly as before, and [`nameable`]
    /// then moves a non-duel job that drew an arena into a kitted dungeon.
    pub(super) fn dungeon_pool(job_type: i64) -> &'static [&'static str] {
        if job_type == 5 {
            DUEL_DUNGEON_TEMPLATES
        } else {
            DUNGEON_TEMPLATES
        }
    }
    const ENEMY_FAMILIES: &[&str] = &[
        "008cf5b0-2590-433b-832e-f2e6f0e0226f",
        "06591d48-8c3a-4f81-a2c6-dba2e7163788",
        "1696d9c0-900f-4829-ae3f-f0441d92a37c",
        "1b2a30db-2871-43a2-bca0-eaa4bd804698",
        "20e856fc-9465-4ffe-8d0a-6118c2eed219",
        "225d747b-9d24-4ffc-9ece-541728b4aef0",
        "31be99a6-8557-4e9b-81e6-5503f900b7d2",
        "33de9f64-8eb6-41d2-b62d-8b7fb3632729",
        "340cd608-31ec-447f-9f72-2162639bff3c",
        "3d932102-3b5c-42ba-b96a-35405752c5a3",
        "3fa0aa97-7a45-4c96-8b62-53e08691f746",
        "4c60bb97-3918-485a-822e-1017d2401dd2",
        "50994925-f050-48b2-8cab-259b0f1a3531",
        "521bb612-587d-4a90-adee-904a48d89c33",
        "6ee657a9-5cc3-45b7-ad14-db8828f7ae2c",
        "7f9c2b46-e6b8-4a65-9caa-f2b952623c23",
        "878febe5-106b-4b48-972a-7debd771a079",
        "8c75bd1f-95a3-47d4-a28c-fdb1fc0de228",
        "90a62106-6294-4456-8206-cf6817995bf8",
        "9137d218-6f05-4e8f-a5e5-1c63c61c95ca",
        "be99402d-c518-4e81-be00-9e2e20e690b0",
        "d14a0ec0-39a5-417f-a21c-8c4840d60a56",
        "de4686d9-f748-40f7-a8d3-7baadb46a695",
        "de8e06be-5403-4fc1-b912-1a5cd9d608a6",
        "ea8096f9-6c1b-4b42-af71-9ceabd7de33d",
    ];
    /// One enemy family a job may name, with what the client needs to field it.
    ///
    /// From the APK's `EnemyData` assets (`blades-capture
    /// reference/game-defs/enemies.json`): `min_level`/`max_level` span the family's
    /// level-banded variants (none of the 22 has a gap), `bosses` is its
    /// `_bossFamilyIds`, and `leads` says whether retail ever made it a job's
    /// primary enemy or boss.
    pub struct JobFamily {
        pub id: &'static str,
        pub min_level: i64,
        pub max_level: i64,
        pub leads: bool,
        pub bosses: &'static [&'static str],
    }

    impl JobFamily {
        fn covers(&self, level: i64) -> bool {
            (self.min_level..=self.max_level).contains(&level)
        }
    }

    /// The families retail's jobs were drawn from (report #337).
    ///
    /// Measured over 417 distinct non-Duel retail jobs (222 captured `/quests`
    /// bodies), 1,251 primary/secondary/boss slots:
    ///
    /// * The three Duelist families in [`ENEMY_FAMILIES`] (Avenger, Barbarian,
    ///   Swordmage) filled **0** slots. They are armoured humans weak to Frost,
    ///   Shock or Fire and resistant to Poison, so drawing them put "humans whose
    ///   weakness keeps changing" into jobs where retail fielded Mercenaries,
    ///   Warmasters and Bandits, every one of them weak to Poison at every level.
    /// * The four critter families (Skeever, Wolf, critter Spider, Wisp) were
    ///   secondaries only: 0 primaries, 0 bosses (`leads: false`).
    /// * Every slot's family has a variant at that slot's level (1,251 of 1,251),
    ///   the boss checked at `difficultyLevel + bossLevelDelta`.
    /// * The boss is in the primary's or the secondary's boss list in 417 of 417
    ///   jobs, and the secret boss in 59 of 59.
    pub const JOB_FAMILIES: &[JobFamily] = &[
        // Bear
        JobFamily { id: "008cf5b0-2590-433b-832e-f2e6f0e0226f", min_level: 4, max_level: 99, leads: true, bosses: &["008cf5b0-2590-433b-832e-f2e6f0e0226f", "06591d48-8c3a-4f81-a2c6-dba2e7163788"] },
        // Spriggan
        JobFamily { id: "06591d48-8c3a-4f81-a2c6-dba2e7163788", min_level: 3, max_level: 99, leads: true, bosses: &["06591d48-8c3a-4f81-a2c6-dba2e7163788"] },
        // Thalmor Agent
        JobFamily { id: "1696d9c0-900f-4829-ae3f-f0441d92a37c", min_level: 6, max_level: 101, leads: true, bosses: &["1696d9c0-900f-4829-ae3f-f0441d92a37c"] },
        // Troll
        JobFamily { id: "1b2a30db-2871-43a2-bca0-eaa4bd804698", min_level: 7, max_level: 99, leads: true, bosses: &["1b2a30db-2871-43a2-bca0-eaa4bd804698", "06591d48-8c3a-4f81-a2c6-dba2e7163788"] },
        // Skeever
        JobFamily { id: "31be99a6-8557-4e9b-81e6-5503f900b7d2", min_level: 2, max_level: 22, leads: false, bosses: &["008cf5b0-2590-433b-832e-f2e6f0e0226f", "8c75bd1f-95a3-47d4-a28c-fdb1fc0de228", "06591d48-8c3a-4f81-a2c6-dba2e7163788"] },
        // Warmaster
        JobFamily { id: "33de9f64-8eb6-41d2-b62d-8b7fb3632729", min_level: 10, max_level: 101, leads: true, bosses: &["33de9f64-8eb6-41d2-b62d-8b7fb3632729"] },
        // Dremora Raider
        JobFamily { id: "340cd608-31ec-447f-9f72-2162639bff3c", min_level: 8, max_level: 106, leads: true, bosses: &["340cd608-31ec-447f-9f72-2162639bff3c", "ea8096f9-6c1b-4b42-af71-9ceabd7de33d", "50994925-f050-48b2-8cab-259b0f1a3531", "1696d9c0-900f-4829-ae3f-f0441d92a37c", "3fa0aa97-7a45-4c96-8b62-53e08691f746"] },
        // Spider
        JobFamily { id: "3d932102-3b5c-42ba-b96a-35405752c5a3", min_level: 9, max_level: 97, leads: false, bosses: &["6ee657a9-5cc3-45b7-ad14-db8828f7ae2c"] },
        // Lich
        JobFamily { id: "3fa0aa97-7a45-4c96-8b62-53e08691f746", min_level: 7, max_level: 100, leads: true, bosses: &["3fa0aa97-7a45-4c96-8b62-53e08691f746"] },
        // Mercenary
        JobFamily { id: "4c60bb97-3918-485a-822e-1017d2401dd2", min_level: 5, max_level: 100, leads: true, bosses: &["4c60bb97-3918-485a-822e-1017d2401dd2", "33de9f64-8eb6-41d2-b62d-8b7fb3632729"] },
        // Necromancer
        JobFamily { id: "50994925-f050-48b2-8cab-259b0f1a3531", min_level: 5, max_level: 100, leads: true, bosses: &["50994925-f050-48b2-8cab-259b0f1a3531", "33de9f64-8eb6-41d2-b62d-8b7fb3632729"] },
        // Wight
        JobFamily { id: "521bb612-587d-4a90-adee-904a48d89c33", min_level: 5, max_level: 99, leads: true, bosses: &["521bb612-587d-4a90-adee-904a48d89c33", "3fa0aa97-7a45-4c96-8b62-53e08691f746"] },
        // Spider
        JobFamily { id: "6ee657a9-5cc3-45b7-ad14-db8828f7ae2c", min_level: 9, max_level: 97, leads: true, bosses: &["6ee657a9-5cc3-45b7-ad14-db8828f7ae2c", "06591d48-8c3a-4f81-a2c6-dba2e7163788", "1b2a30db-2871-43a2-bca0-eaa4bd804698"] },
        // Wolf
        JobFamily { id: "7f9c2b46-e6b8-4a65-9caa-f2b952623c23", min_level: 4, max_level: 30, leads: false, bosses: &["008cf5b0-2590-433b-832e-f2e6f0e0226f", "06591d48-8c3a-4f81-a2c6-dba2e7163788", "33de9f64-8eb6-41d2-b62d-8b7fb3632729"] },
        // Goblin
        JobFamily { id: "8c75bd1f-95a3-47d4-a28c-fdb1fc0de228", min_level: 1, max_level: 101, leads: true, bosses: &["8c75bd1f-95a3-47d4-a28c-fdb1fc0de228", "9137d218-6f05-4e8f-a5e5-1c63c61c95ca"] },
        // Wisp
        JobFamily { id: "90a62106-6294-4456-8206-cf6817995bf8", min_level: 24, max_level: 99, leads: false, bosses: &["be99402d-c518-4e81-be00-9e2e20e690b0"] },
        // Goblin Wizard
        JobFamily { id: "9137d218-6f05-4e8f-a5e5-1c63c61c95ca", min_level: 1, max_level: 100, leads: true, bosses: &["9137d218-6f05-4e8f-a5e5-1c63c61c95ca", "8c75bd1f-95a3-47d4-a28c-fdb1fc0de228"] },
        // Wispmother
        JobFamily { id: "be99402d-c518-4e81-be00-9e2e20e690b0", min_level: 10, max_level: 99, leads: true, bosses: &["be99402d-c518-4e81-be00-9e2e20e690b0", "06591d48-8c3a-4f81-a2c6-dba2e7163788"] },
        // Atronach
        JobFamily { id: "d14a0ec0-39a5-417f-a21c-8c4840d60a56", min_level: 25, max_level: 100, leads: true, bosses: &["d14a0ec0-39a5-417f-a21c-8c4840d60a56", "340cd608-31ec-447f-9f72-2162639bff3c", "ea8096f9-6c1b-4b42-af71-9ceabd7de33d", "9137d218-6f05-4e8f-a5e5-1c63c61c95ca", "50994925-f050-48b2-8cab-259b0f1a3531", "1696d9c0-900f-4829-ae3f-f0441d92a37c", "3fa0aa97-7a45-4c96-8b62-53e08691f746"] },
        // Skeleton
        JobFamily { id: "de4686d9-f748-40f7-a8d3-7baadb46a695", min_level: 1, max_level: 100, leads: true, bosses: &["de4686d9-f748-40f7-a8d3-7baadb46a695", "50994925-f050-48b2-8cab-259b0f1a3531", "3fa0aa97-7a45-4c96-8b62-53e08691f746", "521bb612-587d-4a90-adee-904a48d89c33"] },
        // Bandit
        JobFamily { id: "de8e06be-5403-4fc1-b912-1a5cd9d608a6", min_level: 1, max_level: 98, leads: true, bosses: &["de8e06be-5403-4fc1-b912-1a5cd9d608a6", "4c60bb97-3918-485a-822e-1017d2401dd2", "33de9f64-8eb6-41d2-b62d-8b7fb3632729"] },
        // Dremora Warlock
        JobFamily { id: "ea8096f9-6c1b-4b42-af71-9ceabd7de33d", min_level: 8, max_level: 106, leads: true, bosses: &["ea8096f9-6c1b-4b42-af71-9ceabd7de33d", "340cd608-31ec-447f-9f72-2162639bff3c", "50994925-f050-48b2-8cab-259b0f1a3531", "1696d9c0-900f-4829-ae3f-f0441d92a37c", "3fa0aa97-7a45-4c96-8b62-53e08691f746"] },
    ];

    fn job_family(id: &str) -> Option<&'static JobFamily> {
        JOB_FAMILIES.iter().find(|f| f.id == id)
    }

    /// Separate stream for the family draws, so the main stream's draws -- and
    /// with them every other value on the board -- stay where they were.
    const FAMILY_SIDE_STREAM: u64 = 0xFA41_1E55;

    /// `(primary, secondary, boss, secretBoss)` for a non-Duel job.
    ///
    /// The boss rule is a fit, not a decoded algorithm: one time in five the
    /// primary leads its own pack, otherwise the boss comes from the primary's
    /// or the secondary's boss list with even odds, each list filtered to the
    /// boss level. Against the 417 retail jobs that predicts the boss is in the
    /// primary's list 326 times (retail 333), is the primary itself 201 times
    /// (207) and the secondary 90 times (69).
    pub(super) fn job_families(
        base_seed: u64,
        level: i64,
        boss_level: i64,
        secret_boss_level: i64,
    ) -> (&'static str, &'static str, &'static str, &'static str) {
        let mut rng = Rng::new(base_seed ^ FAMILY_SIDE_STREAM);
        let draw = |rng: &mut Rng, ok: &dyn Fn(&JobFamily) -> bool| -> Option<&'static str> {
            let pool: Vec<&'static str> = JOB_FAMILIES.iter().filter(|f| ok(f)).map(|f| f.id).collect();
            rng.pick(&pool).copied()
        };
        // A level outside every range (none of today's job levels is) keeps the
        // family rules and drops the level rule rather than emitting nothing.
        let primary = draw(&mut rng, &|f| f.leads && f.covers(level))
            .or_else(|| draw(&mut rng, &|f| f.leads))
            .unwrap_or(JOB_FAMILIES[0].id);
        let secondary = draw(&mut rng, &|f| f.covers(level))
            .or_else(|| draw(&mut rng, &|_| true))
            .unwrap_or(primary);
        let boss_of = |rng: &mut Rng, at: i64| -> &'static str {
            let list = |id: &str| -> Vec<&'static str> {
                job_family(id)
                    .map(|f| f.bosses.iter().copied())
                    .into_iter()
                    .flatten()
                    .filter(|b| job_family(b).is_some_and(|b| b.covers(at)))
                    .collect()
            };
            let (own, other) = (list(primary), list(secondary));
            let primary_fits = job_family(primary).is_some_and(|f| f.covers(at));
            let roll = rng.below(10);
            let chosen = if roll < 2 && primary_fits {
                Some(primary)
            } else if roll < 6 || other.is_empty() {
                rng.pick(&own).copied().or_else(|| rng.pick(&other).copied())
            } else {
                rng.pick(&other).copied()
            };
            chosen.unwrap_or(primary)
        };
        let boss = boss_of(&mut rng, boss_level);
        let secret_boss = boss_of(&mut rng, secret_boss_level);
        (primary, secondary, boss, secret_boss)
    }

    const DUEL_BOSSES: &[&str] = &[
        "01d82726-527f-4601-929c-182acd3fa9b7",
        "024b4f81-c7ef-4322-a547-ee863b4c02ad",
        "282b51da-b334-4cab-90fd-ba7fbdea00f1",
        "2f85c042-ab17-47f2-a4b2-385f8626034c",
        "33bbefc6-abb3-48e1-a233-96002d9ca98c",
        "68a30f8a-dd30-4a41-a014-200f16a8ff89",
        "ad5bf23a-2899-40e1-b77b-dd0cb3555176",
        "dadd4e4e-7544-4680-9a73-84208c8ab7a2",
        "ea48eb54-672c-4b28-9c92-60463314ee0d",
    ];
    const GATHER_ITEMS: &[&str] = &[
        "0fab3016-8306-48ee-8268-d3f7bea7d9d2",
        "144a3de0-bc3b-45b4-858e-0c7864ffce52",
        "49a5aed9-3fc2-423a-875c-1e4f3c10f4d8",
        "5fd5015c-43f9-4e25-90cb-e960753842a9",
        "7ea91e7d-3c00-47d8-bf31-6da3aaa008ee",
        "8e7d18af-a9bd-4a3f-964e-ab9f301cdc35",
        "9972b682-4c8d-43ba-90f1-b22f5800b0e9",
        "a885cc70-b2b3-4a28-9e19-2d946e2255e3",
        "b010281a-df63-436c-9396-41eba43665df",
        "d145895e-e222-4cb0-be8a-e297b628173c",
        "d7b5faad-fffe-4717-a75d-bb80ba61b6f5",
        "da767378-8c00-43c1-a5eb-705d7d2f7306",
        "e2a06efd-e77e-4f7b-9138-7dcc64844b62",
        "fa22d326-f218-4c4b-8524-e9481e6066d6",
    ];
    /// The soft-currency reward item ("gold") used by every captured job.
    const REWARD_ITEM_GOLD: &str = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2";

    // -- reward curve, fitted to 802 retail jobs ---------------------------
    //
    // Least-squares over every distinct job in 200 captured `/quests` bodies:
    //
    //     rewardXp         ~ 18.92 * difficultyLevel + 47.7   (n = 802)
    //     rewardItemCount  ~ 19.25 * difficultyLevel + 107.2  (n = 654)
    //
    // The previous constants (15 and 30, invented) underpaid XP by about a fifth
    // and overpaid gold by about half.
    /// RETIRED: the spread the old fitted XP line drew. A job's XP is now retail's
    /// exact value for its difficulty ([`job_reward_xp`]); the draw is still made so
    /// every later roll (gold, name, gather item, duel boss) keeps its position.
    const XP_JITTER: u64 = 90;

    /// Retail's job `rewardXp` for `difficultyLevel` 1..=84 (index 0 = difficulty 1).
    ///
    /// MEASURED (tracker #337): 1,443 distinct retail jobs in the pre-shutdown
    /// `/quests` captures (2026-05-02 .. 2026-06-29), and `rewardXp` is a pure
    /// function of `difficultyLevel` — **0 conflicts** at any of the 83 difficulties
    /// seen. Character level, `initialEPL`, job type, pool and secret room do not
    /// move it. 83 is the only gap; 51..=84 is exactly `+10` per level, so it is 1606.
    const RETAIL_JOB_XP: [u64; 84] = [
        50, 60, 70, 82, 94, 106, 120, 134, 148, 164, // 1-10
        180, 196, 214, 232, 250, 270, 290, 310, 332, 354, // 11-20
        376, 400, 424, 448, 474, 500, 528, 556, 586, 616, // 21-30
        648, 680, 714, 748, 784, 820, 846, 874, 902, 932, // 31-40
        962, 994, 1026, 1060, 1094, 1128, 1164, 1200, 1238, 1276, // 41-50
        1286, 1296, 1306, 1316, 1326, 1336, 1346, 1356, 1366, 1376, // 51-60
        1386, 1396, 1406, 1416, 1426, 1436, 1446, 1456, 1466, 1476, // 61-70
        1486, 1496, 1506, 1516, 1526, 1536, 1546, 1556, 1566, 1576, // 71-80
        1586, 1596, 1606, 1616, // 81-84
    ];

    /// The XP a job of `difficulty` pays on `/complete` — retail's table, not a fit.
    /// Below 1 pays difficulty 1's; above 84 (never seen in retail) continues the
    /// `+10` per level that 51..=84 follows without exception.
    pub fn job_reward_xp(difficulty: i64) -> u64 {
        let top = RETAIL_JOB_XP.len() as i64;
        let d = difficulty.max(1);
        if d <= top {
            RETAIL_JOB_XP[(d - 1) as usize]
        } else {
            RETAIL_JOB_XP[RETAIL_JOB_XP.len() - 1] + 10 * (d - top) as u64
        }
    }
    /// Gold per point of `difficultyLevel`.
    const GOLD_PER_DIFFICULTY: u64 = 19;
    /// Gold a difficulty-0 job would pay — the fitted intercept.
    const GOLD_BASE: u64 = 107;
    /// Spread around the fitted gold line.
    const GOLD_JITTER: u64 = 180;
    /// RETIRED: the old roll's chance of a job paying no gold (148 of 802). Those
    /// 148 were the featured and boss jobs, which pay gems instead (#306); the draw
    /// is still made so later rolls keep their positions in the stream.
    const ZERO_GOLD_PER_MILLE: u64 = 185;
    /// Salt for the side stream a standard job takes its gold jitter from when the
    /// retired zero-gold draw hit.
    const GOLD_SIDE_STREAM: u64 = 0x601D_5EED;
    /// Gems a featured (`presentation` 1) job pays: 84 of 84 distinct retail jobs,
    /// every level band 1-100.
    pub const FEATURED_JOB_GEMS: u64 = 4;
    /// Gems a boss (`presentation` 2) job pays: 27 of 27 distinct retail jobs.
    pub const BOSS_JOB_GEMS: u64 = 12;

    /// A job's `rewardGemCount`, which retail fixes by its pool's presentation and
    /// never by the secret room (0 of 109 standard secret-room jobs paid gems).
    pub fn job_gem_reward(presentation: i64) -> u64 {
        match presentation {
            1 => FEATURED_JOB_GEMS,
            2 => BOSS_JOB_GEMS,
            _ => 0,
        }
    }

    /// Objective template IDs are fixed per job type in the captures.
    fn objective_ids(job_type: i64) -> &'static [&'static str] {
        match job_type {
            0 => &[
                "fe67a8c1-b107-44de-8e6a-c76e259fd42d",
                "8b425eba-67ff-4d38-ba3b-ffa2e8493954",
            ],
            1 => &[
                "33af0174-0c6e-4907-a4c6-77fa9caff640",
                "919c2ad0-0b07-4fd2-a690-8d36be2e311b",
            ],
            3 => &[
                "c1ac35b0-4bda-4741-8115-0d3345d63ce6",
                "51cdee5a-82d9-46bd-9655-64ae0294c310",
            ],
            4 => &[
                "0e54c204-300d-40c1-b9e1-674380dfa330",
                "bdb8cc31-d9e1-409c-9a57-0014af59d430",
            ],
            5 => &["091311ed-5e00-40d7-8720-8428407291e0"],
            // type 2 (Clear) wasn't captured; reuse the Defeat objective pair.
            _ => &[
                "fe67a8c1-b107-44de-8e6a-c76e259fd42d",
                "8b425eba-67ff-4d38-ba3b-ffa2e8493954",
            ],
        }
    }

    /// Number of distinct localization variants per job-type name key.
    fn name_variant_count(job_type: i64) -> u64 {
        match job_type {
            0 => 13, // Defeat.001..013
            1 => 11, // Explore
            2 => 6,  // Clear (estimate)
            3 => 6,  // Rescue
            4 => 8,  // Gather
            5 => 8,  // Duel
            _ => 1,
        }
    }
    // -- job localization: dynamicElements ---------------------------------
    //
    // Retail fills `questName`/`questDescription` with a `dynamicElements` list
    // whose SHAPE is fixed per jobType and whose values name the enemy, item,
    // location or dungeon kit the job refers to. The rule below was mined from
    // 2,149 distinct job entries in 1,725 captured `/quests` responses:
    //
    //   0 Defeat  name [enemy, enemyGroup, location]  desc [primaryEnemyCount, enemyPlural]
    //   1 Explore name [location]                     desc [dungeon kit name]
    //   3 Rescue  name [location]                     desc [rescueNpcCount]
    //   4 Gather  name [item, count, location]        desc [item, count, location]
    //   5 Duel    name [npc, location]                desc [npc]
    //
    // The integers are exact: Defeat's is `primaryEnemyCount` (413/418 observed),
    // Rescue's `rescueNpcCount` (496/496), Gather's `gatherItemCount`.
    //
    // The LOCATION is not a function of the dungeon template -- 0 of 12 templates
    // map to a single name, each shows twelve -- so it is rolled from that
    // template's observed set, which is what retail's own roll does.
    //
    // An EMPTY name list is not a safe fallback, whatever retail's corpus holds
    // (the 2026-06-07 snapshot's 463 distinct jobs have none): every
    // `UI.Jobs.Names.*` string has a placeholder and the client formats it
    // unguarded, so an empty list wedges the quest map. `nameable` keeps the draws
    // inside what this table can name.
    static JOB_LOCALIZATION_RAW: &str = include_str!("job_localization.json");

    fn job_localization() -> &'static Value {
        static TABLE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        TABLE.get_or_init(|| {
            serde_json::from_str(JOB_LOCALIZATION_RAW).unwrap_or_else(|e| {
                log::error!("[jobs] job_localization.json is malformed: {e}");
                json!({})
            })
        })
    }

    fn loc_elem(key: &str) -> Value {
        json!({ "type": "LOCALIZATION_ID", "localizationValue": key })
    }
    fn int_elem(n: i64) -> Value {
        json!({ "type": "INTEGER", "intValue": n })
    }

    /// One `UI.Jobs.Location.Name.*` for this dungeon template, rolled from the
    /// set retail used for it. `None` when the template is not in the table.
    fn pick_location(rng: &mut Rng, dungeon_template_id: &str) -> Option<String> {
        let list = job_localization()
            .get("dungeons")?
            .get(dungeon_template_id)?
            .get("locations")?
            .as_array()?;
        let v = rng.pick(list)?;
        v.as_str().map(|s| s.to_string())
    }

    fn kit_name(dungeon_template_id: &str) -> Option<String> {
        job_localization()
            .get("dungeons")?
            .get(dungeon_template_id)?
            .get("kitName")?
            .as_str()
            .map(|s| s.to_string())
    }

    fn enemy_str(family_id: &str, field: &str) -> Option<String> {
        job_localization()
            .get("enemyFamilies")?
            .get(family_id)?
            .get(field)?
            .as_str()
            .map(|s| s.to_string())
    }

    fn item_key(item_id: &str) -> Option<String> {
        job_localization()
            .get("items")?
            .get(item_id)?
            .as_str()
            .map(|s| s.to_string())
    }

    fn pick_npc(rng: &mut Rng) -> Option<String> {
        let list = job_localization().get("npcs")?.as_array()?;
        rng.pick(list)?.as_str().map(|s| s.to_string())
    }

    /// Re-point a Defeat's primary family, or an Explore's dungeon template, at
    /// one the localization table can name.
    ///
    /// Every `UI.Jobs.Names.*` string carries a `{n}` placeholder, and the client's
    /// `Quest.BuildLocalizedQuestName` feeds it to `String.Format` unguarded. An
    /// EMPTY name list therefore throws `FormatException` inside
    /// `QuestMapMenuController.SetupQuestMap` (via
    /// `QuestMapMarkerJobDetailsProvider.SetupForID`), and the whole quest map --
    /// QUESTS, JOBS and EVENTS -- spins forever. That is what wedged every board
    /// rolled at the 2026-09-25 05:00 reset that drew one of these ids.
    ///
    /// The draws come from pools harvested across every role an id played, so
    /// they include ids retail never put in these slots. In the 463 distinct
    /// retail jobs of the 2026-06-07 capture snapshot: 0 of 417 non-duel jobs had
    /// a primary family outside the table (the seven outside it are the duelist
    /// and critter families), and the five templates with no kit name are the
    /// Arena templates, used by Duels (46/46) and by nothing else.
    ///
    /// That last fact holds for EVERY non-duel type, not just Explore. A Defeat,
    /// Rescue or Gather drawn into an arena still had a name (the arena's title),
    /// so the quest map rendered it, but the client could not start it: tracker
    /// #306's "Unendlich viel: Holz" was a Gather in `JobArenaVariant_06` and hung
    /// on tap with no `…/dungeons/current/enter` ever sent. Across 369 characters'
    /// boards from 2026-09-15 to 2026-10-02, 29% of Defeat/Rescue/Gather jobs sat
    /// in an arena, yet 0 of the 17 such jobs players entered did (p ≈ 0.003). So
    /// any non-duel job leaves an arena for a kitted dungeon; the kitted set is
    /// exactly the 12 templates retail used for non-duel jobs.
    ///
    /// The replacement is keyed on the job's own seed, so it is deterministic and
    /// consumes no rng draw: every other value on the board stays what it was.
    fn nameable<'a>(
        job_type: i64,
        dungeon: &'a str,
        prim_fam: &'a str,
        base_seed: u64,
    ) -> (&'a str, &'a str) {
        let has_kit = |d: &str| {
            let has_location = job_localization()["dungeons"][d]["locations"]
                .as_array()
                .is_some_and(|l| !l.is_empty());
            kit_name(d).is_some() && has_location
        };
        let has_name = |f: &str| enemy_str(f, "name").is_some() && enemy_str(f, "plural").is_some();
        let swap = |pool: &'a [&'a str], ok: &dyn Fn(&str) -> bool| -> Option<&'a str> {
            let usable: Vec<&'a str> = pool.iter().copied().filter(|x| ok(x)).collect();
            if usable.is_empty() {
                None
            } else {
                Some(usable[(base_seed % usable.len() as u64) as usize])
            }
        };
        let dungeon = if job_type != 5 && !has_kit(dungeon) {
            swap(DUNGEON_TEMPLATES, &has_kit).unwrap_or(dungeon)
        } else {
            dungeon
        };
        let prim_fam = if job_type == 0 && !has_name(prim_fam) {
            swap(ENEMY_FAMILIES, &has_name).unwrap_or(prim_fam)
        } else {
            prim_fam
        };
        (dungeon, prim_fam)
    }

    /// `(nameElements, descriptionElements)` for one rolled job.
    ///
    /// Returns empty lists rather than partial ones: retail never sent a
    /// half-filled list. But an empty NAME list is fatal to the quest map (see
    /// [`nameable`]), so `roll_job` only reaches the empty arm through a table
    /// that has lost its entries, and it logs when it does.
    fn dynamic_elements(
        rng: &mut Rng,
        job_type: i64,
        setup: &serde_json::Map<String, Value>,
    ) -> (Vec<Value>, Vec<Value>) {
        let s = |k: &str| {
            setup
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let n = |k: &str| setup.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        let dungeon = s("dungeonTemplateId");
        let empty = (Vec::new(), Vec::new());

        match job_type {
            0 => {
                let fam = s("primaryEnemyFamilyId");
                let (Some(name), Some(plural), Some(loc)) = (
                    enemy_str(&fam, "name"),
                    enemy_str(&fam, "plural"),
                    pick_location(rng, &dungeon),
                ) else {
                    return empty;
                };
                // Retail uses the GROUP name in the second slot where the family has
                // one and repeats the enemy name where it does not (216 vs 202).
                let group = enemy_str(&fam, "group").unwrap_or_else(|| name.clone());
                (
                    vec![loc_elem(&name), loc_elem(&group), loc_elem(&loc)],
                    vec![int_elem(n("primaryEnemyCount")), loc_elem(&plural)],
                )
            }
            1 => {
                let (Some(loc), Some(kit)) = (pick_location(rng, &dungeon), kit_name(&dungeon))
                else {
                    return empty;
                };
                (vec![loc_elem(&loc)], vec![loc_elem(&kit)])
            }
            3 => {
                let Some(loc) = pick_location(rng, &dungeon) else {
                    return empty;
                };
                (vec![loc_elem(&loc)], vec![int_elem(n("rescueNpcCount"))])
            }
            4 => {
                let (Some(item), Some(loc)) =
                    (item_key(&s("gatherItemId")), pick_location(rng, &dungeon))
                else {
                    return empty;
                };
                let count = int_elem(n("gatherItemCount"));
                let els = vec![loc_elem(&item), count, loc_elem(&loc)];
                (els.clone(), els)
            }
            5 => {
                let (Some(npc), Some(loc)) = (pick_npc(rng), pick_location(rng, &dungeon)) else {
                    return empty;
                };
                (vec![loc_elem(&npc), loc_elem(&loc)], vec![loc_elem(&npc)])
            }
            _ => empty,
        }
    }

    fn name_prefix(job_type: i64) -> &'static str {
        match job_type {
            0 => "Defeat",
            1 => "Explore",
            2 => "Clear",
            3 => "Rescue",
            4 => "Gather",
            5 => "Duel",
            _ => "Defeat",
        }
    }

    // -- small Value helpers (never panic) ---------------------------------
    fn get_u64(v: &Value, key: &str, def: u64) -> u64 {
        v.get(key).and_then(|x| x.as_u64()).unwrap_or(def)
    }
    fn get_i64(v: &Value, key: &str, def: i64) -> i64 {
        v.get(key).and_then(|x| x.as_i64()).unwrap_or(def)
    }
    fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
        v.get(key).and_then(|x| x.as_str())
    }

    /// The current daily-reset boundary (epoch secs of the most recent
    /// `resetHour:resetMinute` UTC on or before `now`). Reads the reset time from
    /// globals/pool recurrence; defaults to 05:00 UTC.
    pub fn current_reset_boundary(pools_def: &Value, now: u64) -> u64 {
        let (hour, minute) = daily_reset_hm(pools_def);
        last_reset_at_or_before(now, hour, minute)
    }

    fn daily_reset_hm(pools_def: &Value) -> (u64, u64) {
        if let Some(g) = pools_def.get("globals") {
            if let Some(h) = g.get("dailyJobsRefreshTimeHour").and_then(|x| x.as_u64()) {
                let m = g.get("dailyJobsRefreshTimeMinute").and_then(|x| x.as_u64());
                return (h, m.unwrap_or(0));
            }
        }
        if let Some(pools) = pools_def.get("jobPools").and_then(|p| p.as_array()) {
            for p in pools {
                if let Some(rec) = p.get("recurrence") {
                    if let Some(h) = rec.get("resetHour").and_then(|x| x.as_u64()) {
                        return (
                            h,
                            rec.get("resetMinute").and_then(|x| x.as_u64()).unwrap_or(0),
                        );
                    }
                }
            }
        }
        (DEFAULT_RESET_HOUR, 0)
    }

    /// Epoch secs of the most recent `hour:minute` UTC boundary <= `now`.
    fn last_reset_at_or_before(now: u64, hour: u64, minute: u64) -> u64 {
        let day_start = (now / SECS_PER_DAY) * SECS_PER_DAY; // 00:00 UTC of now's day
        let today_reset = day_start + hour * 3600 + minute * 60;
        if now >= today_reset {
            today_reset
        } else {
            today_reset.saturating_sub(SECS_PER_DAY)
        }
    }

    /// Weekday index (0 = Sunday .. 6 = Saturday) of an epoch-secs instant (UTC).
    fn weekday_sun0(epoch: u64) -> u64 {
        // 1970-01-01 was a Thursday (=4 in Sun0 indexing).
        ((epoch / SECS_PER_DAY) + 4) % 7
    }

    /// Advance the difficulty-cycle index, wrapping the `globals.difficultyCycle`
    /// length (defaults to a full wrap at 80 if absent). Purely a rotation counter.
    pub fn next_cycle_index(pools_def: &Value, cur: i64) -> i64 {
        let len = pools_def
            .get("globals")
            .and_then(|g| g.get("difficultyCycle"))
            .and_then(|c| c.as_array())
            .map(|a| a.len() as i64)
            .filter(|n| *n > 0)
            .unwrap_or(80);
        // Advance by the max jobs rolled per reset (4) so the cycle drifts like prod.
        (cur + 4).rem_euclid(len)
    }

    /// Whether a stored `Quest` row is one of our JOB rows (by sentinel gldQuestId).
    pub fn is_job_row(q: &Quest) -> bool {
        q.gld_quest_id == JOB_SENTINEL_GLD
    }

    /// The `DungeonGeneratedData` for one rolled job.
    ///
    /// Every job draws from the SAME dungeon ([`JOB_SPAWN_GROUPS_REFERENCE`]) — that is
    /// how retail does it — with the enemies scaled to the job's own `difficultyLevel`.
    /// A job's `difficultyLevel` IS its enemy level (retail: a level-48 character's board
    /// carried jobs at 42 and 46), so it goes straight in; the XP per enemy comes from the
    /// shared table rather than a second copy of `100 * level` here.
    ///
    /// `scaling` must be the LOADED table. This took `QuestLevelScaling::default()`,
    /// which was harmless while the default *was* the formula and became a silent
    /// bug the moment the real numbers moved into `quests_daily.json`: an empty
    /// table falls through to `100 * enemyLevel`, so every job on every board kept
    /// paying the old XP while quests paid retail's. A fresh character's board is
    /// all jobs, so this was the first thing a new player saw.
    ///
    /// Returns `None` only when `parsed.json` is missing the reference dungeon, which the
    /// caller treats as "no entry" rather than an error — same policy as the quest path.
    pub fn generated_data_for_job(
        game_data: &GameData,
        job: &Value,
        scaling: &QuestLevelScaling,
    ) -> Option<DungeonGeneratedData> {
        let enemy_level = get_i64(job, "difficultyLevel", 1).max(1);
        let given_xp = scaling.given_xp(enemy_level);
        // Rolled per JOB (#337). Every job used to roll the reference dungeon's one
        // fixed seed, so every job at a level carried the same corpse and floor
        // loot position by position -- one board's secondary spawner 0 dropped
        // Major Aversion to Poison in 9 of 9 jobs -- and the same two chests.
        // Retail's loot differs job to job: 409 distinct (level, first primary
        // enemy's loot) pairs among 417 captured jobs. Keyed on the job's own id
        // and seed, so the board and the stored row describe one dungeon.
        let job_id = get_str(job, "questId")
            .and_then(|q| Uuid::parse_str(q).ok())
            .unwrap_or_default();
        let run_seed =
            blades_lib::util::dungeon::run_loot_seed(&job_id, get_i64(job, "seed", 0) as u64);
        let mut data = blades_lib::util::dungeon::generate_for_dungeon_with_seed(
            game_data,
            &JOB_SPAWN_GROUPS_REFERENCE,
            run_seed,
            enemy_level,
            given_xp,
        )?;

        let setup = job.get("jobSetup").cloned().unwrap_or(Value::Null);
        if get_i64(&setup, "jobType", -1) == 5 {
            // A Duel has no ordinary primary/secondary packs, secret boss, or
            // floor-item rolls. The committed retail Duel is discriminating:
            // normal jobs in the same response do carry those groups.
            data.enemy_generated_data
                .retain(|id, _| DUEL_ENEMY_SPAWN_GROUPS.contains(id));
            data.item_generated_data.clear();

            // Retail levels the two fixed encounters at difficultyLevel and the
            // boss at difficultyLevel + bossLevelDelta (43 and 51 in the sample).
            let boss_level = enemy_level + get_i64(&setup, "bossLevelDelta", 0);
            for (spawn_id, spawners) in &mut data.enemy_generated_data {
                let level = if *spawn_id == JOB_BOSS_SPAWN_GROUP {
                    boss_level
                } else {
                    enemy_level
                };
                let xp = scaling.given_xp(level);
                for enemies in spawners {
                    for enemy in enemies {
                        enemy.enemy_level = level;
                        enemy.given_xp = xp;
                    }
                }
            }
        } else {
            // An ORDINARY job needs the same treatment for the same reason. See the
            // constants above for the three measured rules and their sample counts.

            // 1. The secret-room boss only exists when the job has one.
            if setup
                .get("secretBossEnemyFamilyId")
                .is_none_or(Value::is_null)
            {
                data.enemy_generated_data
                    .remove(&JOB_SECRET_BOSS_SPAWN_GROUP);
            }

            // 2. The packs carry the job's OWN declared counts, not the reference
            //    dungeon's authored 6 and 4.
            for (group, key) in [
                (JOB_PRIMARY_SPAWN_GROUP, "primaryEnemyCount"),
                (JOB_SECONDARY_SPAWN_GROUP, "secondaryEnemyCount"),
            ] {
                let want = get_i64(&setup, key, -1);
                if want < 0 {
                    continue;
                }
                if let Some(spawners) = data.enemy_generated_data.get_mut(&group) {
                    spawners.truncate(want as usize);
                }
            }

            // 3. The Gather pickup only exists on a Gather job.
            if setup.get("gatherItemId").is_none_or(Value::is_null) {
                data.item_generated_data
                    .remove(&JOB_GATHER_ITEM_SPAWN_GROUP);
            }

            // 4. The floor-item groups are a RARITY PICK, not the whole list. See
            //    the constants above for the four measured rules.
            let mut rng = Rng::new(get_i64(job, "seed", 0) as u64);
            // Exactly one of the two secret-room rarity groups (932:463 measured).
            let drop_secret = if rng.below(1395) < 932 {
                JOB_SECRET_ITEM_R3
            } else {
                JOB_SECRET_ITEM_R2
            };
            data.item_generated_data.remove(&drop_secret);
            // At most one of the two main rarity groups (688 : 346 : 361 none).
            let roll = rng.below(1395);
            if roll < 688 {
                data.item_generated_data.remove(&JOB_ITEM_R3);
            } else if roll < 688 + 346 {
                data.item_generated_data.remove(&JOB_ITEM_R2);
            } else {
                data.item_generated_data.remove(&JOB_ITEM_R2);
                data.item_generated_data.remove(&JOB_ITEM_R3);
            }

            // 4. And the boss is levelled, as it already is for a Duel. Every enemy
            //    was flat at `difficultyLevel`, leaving non-duel bosses 4-8 levels
            //    under retail (1395/1395 carry `difficultyLevel + bossLevelDelta`).
            let boss_level = enemy_level + get_i64(&setup, "bossLevelDelta", 0);
            if let Some(spawners) = data.enemy_generated_data.get_mut(&JOB_BOSS_SPAWN_GROUP) {
                let xp = scaling.given_xp(boss_level);
                for enemies in spawners {
                    for enemy in enemies {
                        enemy.enemy_level = boss_level;
                        enemy.given_xp = xp;
                    }
                }
            }
        }
        // Retail stamps `version: 1` on all ten generated-data entries in the captured
        // body. `generate_for_dungeon` hard-codes 0 because that is what our story quests
        // have always shipped and they work; bumping it there would change their wire
        // output for no measured reason, so the job path — which has a measured value —
        // sets its own.
        data.version = 1;
        Some(data)
    }

    /// The `dungeonGeneratedDataList` entries for a whole board.
    ///
    /// Retail's list covers jobs as well as quests: in the captured body all 6 job ids and
    /// both story-quest ids were present (10 entries = 6 jobs + 2 quests + 2 events). We
    /// used to send none for jobs, so the client listed a job on the map and then waited
    /// forever for data that was never coming — the same failure as report #62, and the
    /// reason deleting `job_pools.json` "fixed" the hang: no jobs, no unresolvable ids.
    ///
    /// Derived from the freshly rolled board rather than from the stored rows on purpose.
    /// The board is the set the client is actually told about, so this cannot drift out of
    /// step with `jobs[]`, and it heals characters whose rows were written by the old code
    /// with a NULL `generated_data` (their rows are only rewritten at the next daily
    /// rotation). The row is still populated by [`job_quest_db_entry`] for /accept and the
    /// dungeon-enter path — and because both sides derive from the same reference dungeon
    /// and the same `difficultyLevel`, the two agree by construction.
    pub fn job_generated_data_list(
        game_data: &GameData,
        jobs: &[Value],
        scaling: &QuestLevelScaling,
    ) -> Vec<DungeonGeneratedDataWithId> {
        jobs.iter()
            .filter_map(|job| {
                let quest_id = Uuid::parse_str(get_str(job, "questId")?).ok()?;
                Some(DungeonGeneratedDataWithId {
                    quest_id,
                    inner: generated_data_for_job(game_data, job, scaling)?,
                })
            })
            .collect()
    }

    /// What a job pays on completion, read off the job's own `jobSetup`.
    ///
    /// `rewardXp` becomes `characterXp` and `rewardItemCount` of `rewardItemId`
    /// becomes a **currency** credit. Both halves are measured, not assumed: in the
    /// retail corpus job `1385706b-…` declared `rewardXp: 1526` / `rewardItemCount:
    /// 1000` and its `/complete` paid exactly `characterXp: 1526` and
    /// `currencies: {gold: 1000}`; `9ba20667-…` declared 1586/1000 and paid
    /// 1586/1000. `rewardItemId` was gold on 802 of 802 sampled jobs, and gold
    /// arrives under `currencies` (never `stackableItems`) in every job completion
    /// in the sample.
    ///
    /// A zero count is a real retail value — 0 gold appears at many difficulties —
    /// so it is skipped rather than credited, which keeps the wire's
    /// `skip_serializing_if` shape (`currencies` omitted, not `{gold: 0}`).
    pub fn job_completion_reward(job: &Value) -> RewardGrant {
        let setup = job.get("jobSetup").cloned().unwrap_or(Value::Null);
        let mut reward = RewardGrant {
            character_xp: get_u64(&setup, "rewardXp", 0),
            ..Default::default()
        };
        let count = get_u64(&setup, "rewardItemCount", 0);
        if count > 0 {
            if let Some(id) = get_str(&setup, "rewardItemId").and_then(|s| Uuid::parse_str(s).ok())
            {
                reward.currencies.insert(id, count);
            }
        }
        // Gems are a currency too: retail's /complete for a 4-gem featured job paid
        // `currencies: {gems: 4}` (captures 9303, 11388, 18482) and for a 12-gem
        // boss job `{gems: 12}` (11253, 37130).
        let gems = get_u64(&setup, "rewardGemCount", 0);
        if gems > 0 {
            *reward.currencies.entry(blades_lib::economy::GEMS).or_insert(0) += gems;
        }
        reward
    }

    /// Build the storable `QuestDbEntry` for a generated job Value. The row is a
    /// plain `Quest` (type Normal, sentinel gldQuestId) carrying the job's
    /// objective statuses + difficulty + seed, so /objectives + /complete resolve
    /// it. The rich `jobSetup` lives only in the regenerated board.
    ///
    /// The row also carries the job's generated data. It used to store `None`, which left
    /// /accept handing the client a job with no dungeon data — the accept-path half of the
    /// same hang.
    pub fn job_quest_db_entry(
        job: &Value,
        character_id: Uuid,
        game_data: &GameData,
        scaling: &QuestLevelScaling,
    ) -> Option<QuestDbEntry> {
        let quest_id = Uuid::parse_str(get_str(job, "questId")?).ok()?;
        let mut objective_statuses = HashMap::new();
        if let Some(obj) = job.get("objectiveStatuses").and_then(|v| v.as_object()) {
            for k in obj.keys() {
                if let Ok(oid) = Uuid::parse_str(k) {
                    objective_statuses.insert(
                        oid,
                        ObjectiveStatus {
                            status: QuestStatus::Active,
                            progress: 0.0,
                            completed: false,
                        },
                    );
                }
            }
        }
        let quest = Quest {
            version: get_u64(job, "version", 0),
            r#type: QuestType::Normal,
            objective_statuses,
            difficulty_level: get_i64(job, "difficultyLevel", 1),
            seed: get_i64(job, "seed", 0).into(),
            gld_quest_id: JOB_SENTINEL_GLD,
            game_event_quest_data: None,
            rewards: None,
            final_reward: None,
            job_reward: Some(job_completion_reward(job)),
            completed: false,
        };
        Some(QuestDbEntry {
            id: quest_id,
            character_id,
            info: JsonDbWrapper(quest),
            generated_data: JsonDbWrapper(generated_data_for_job(game_data, job, scaling)),
            dungeon_state: None,
        })
    }

    /// How many jobs a pool contributes right now (0 when the pool is dormant),
    /// mirroring the captured behaviour:
    ///   * standard/daily      → maxActive jobs, always active.
    ///   * boss/special weekly → 1 job, always active.
    ///   * featured weekly     → 1 job, only on its `dayOfWeek` window; the featured
    ///     pool for weekday D is active during the game-day that began at D's reset,
    ///     i.e. current game-weekday == (D+1) mod 7.
    ///   * featured/daily      → 0 (dormant in prod).
    fn pool_active_count(pool: &Value, reset_boundary: u64) -> u64 {
        let rec = pool.get("recurrence").cloned().unwrap_or(Value::Null);
        let rec_type = get_i64(&rec, "type", 1);
        let presentation = get_i64(pool, "presentation", 0);
        let max_active = get_u64(pool, "maxActive", 1);
        match presentation {
            0 => max_active.max(1),
            2 => 1,
            _ => {
                if rec_type == 1 {
                    0
                } else {
                    let target_dow = get_i64(&rec, "dayOfWeek", 0).rem_euclid(7) as u64;
                    let cur_dow = weekday_sun0(reset_boundary);
                    if cur_dow == (target_dow + 1) % 7 {
                        1
                    } else {
                        0
                    }
                }
            }
        }
    }

    /// Compute a pool's `{endTime, nextStartTime}` timers relative to `now`.
    fn pool_timers(pool: &Value, now: u64, active_count: u64) -> (u64, u64) {
        let rec = pool.get("recurrence").cloned().unwrap_or(Value::Null);
        let rec_type = get_i64(&rec, "type", 1);
        let hour = get_u64(&rec, "resetHour", DEFAULT_RESET_HOUR);
        let minute = get_u64(&rec, "resetMinute", 0);
        let today_reset = last_reset_at_or_before(now, hour, minute);
        let next_daily = today_reset + SECS_PER_DAY;
        let presentation = get_i64(pool, "presentation", 0);
        if rec_type == 1 {
            // daily pool: window ends at the next daily reset.
            (next_daily, next_daily)
        } else {
            // weekly pool.
            let target_dow = get_i64(&rec, "dayOfWeek", 0).rem_euclid(7) as u64;
            let next_target = next_weekday_reset(now, target_dow, hour, minute);
            if presentation == 2 {
                (next_target, next_target)
            } else if active_count > 0 {
                (next_daily, next_target)
            } else {
                (0, next_target)
            }
        }
    }

    /// Epoch secs of the next `hour:minute` reset that falls on `target_dow`
    /// (0=Sun..6=Sat), strictly after `now`.
    fn next_weekday_reset(now: u64, target_dow: u64, hour: u64, minute: u64) -> u64 {
        let base = last_reset_at_or_before(now, hour, minute);
        let base_dow = weekday_sun0(base);
        let days_ahead = (target_dow + 7 - base_dow) % 7;
        let candidate = base + days_ahead * SECS_PER_DAY;
        if candidate <= now {
            candidate + SECS_PER_WEEK
        } else {
            candidate
        }
    }
    const SECS_PER_WEEK: u64 = 604_800;

    /// Retail's job generator does not roll from the raw character level at high
    /// levels. It first derives an effective player level (`jobSetup.initialEPL`),
    /// then applies the per-type difficulty offsets to that baseline.
    fn job_initial_epl(pools_def: &Value, level: u16) -> i64 {
        let level = level.max(1) as i64;
        let Some(obj) = pools_def
            .get("globals")
            .and_then(|g| g.get("initialEplByPlayerLevel"))
            .and_then(Value::as_object)
        else {
            return level;
        };

        obj.iter()
            .filter_map(|(k, v)| Some((k.parse::<i64>().ok()?, v.as_i64()?)))
            .filter(|(k, _)| *k <= level)
            .max_by_key(|(k, _)| *k)
            .map(|(_, epl)| epl.max(1))
            .unwrap_or(level)
    }

    /// The difficulty band (0 = very easy .. 4 = very hard) retail assigns to the
    /// job at `cycle_pos`: `globals.difficultyCycle[cycle_pos mod len]`, the
    /// character's `jobDifficultyCycleIndex` plus the job's place on the board.
    /// `None` when the data carries no cycle.
    fn cycle_band(pools_def: &Value, cycle_pos: i64) -> Option<usize> {
        let cycle = pools_def
            .get("globals")
            .and_then(|g| g.get("difficultyCycle"))
            .and_then(Value::as_array)
            .filter(|c| !c.is_empty())?;
        let band = cycle[cycle_pos.rem_euclid(cycle.len() as i64) as usize].as_u64()?;
        Some((band as usize).min(4))
    }

    /// Difficulty-level roll for a job: effective player level offset by the
    /// per-type, per-level difficulty distribution from `perTypeDifficulty`
    /// (clamped to a floor of 1).
    ///
    /// Tracker #313: the offset is drawn from ONE band of that distribution, the
    /// band the difficulty cycle names, not uniformly over the whole
    /// `veryEasyMin..=veryHardMax` span. Retail (2026-06-07 snapshot): 37 of 42
    /// consecutive board refreshes carry exactly the bands
    /// `difficultyCycle[prevIndex..newIndex]` predicts when measured against
    /// `initialEPL`, and the 157 jobs seen at levels 1-20 sit at 31% very easy,
    /// 43% easy, 17% normal, 8% hard, 1% very hard (the cycle: 25/50/15/7.5/2.5).
    /// The uniform roll put half of every board at hard or very hard.
    fn roll_difficulty(
        pools_def: &Value,
        job_type: i64,
        base_level: i64,
        band: Option<usize>,
        rng: &mut Rng,
    ) -> i64 {
        let level = base_level.max(1);
        let (mut lo, mut hi) = pools_def
            .get("globals")
            .and_then(|g| g.get("baseJobDifficultyRange"))
            .map(|r| (get_i64(r, "min", -2), get_i64(r, "max", 9)))
            .unwrap_or((-2, 9));
        if let Some(arr) = pools_def
            .get("perTypeDifficulty")
            .and_then(|a| a.as_array())
        {
            if let Some(entry) = arr.iter().find(|e| get_i64(e, "jobType", -1) == job_type) {
                if let Some(by_level) = entry.get("difficultyByLevel").and_then(|a| a.as_array()) {
                    let mut best: Option<&Value> = None;
                    for row in by_level {
                        if get_i64(row, "level", i64::MAX) <= level {
                            best = Some(row);
                        }
                    }
                    if let Some(row) = best.or_else(|| by_level.get(0)) {
                        lo = get_i64(row, "veryEasyMin", lo);
                        hi = get_i64(row, "veryHardMax", hi);
                        if let Some(band) = band {
                            (lo, hi) = band_range(row, band, lo, hi);
                        }
                    }
                }
            }
        }
        let offset = rng.range_incl(lo, hi);
        (level + offset).max(1)
    }

    /// `[min, max]` offset of one band of a `difficultyByLevel` row: from that
    /// band's lower bound to one below the next band's (very hard ends at
    /// `veryHardMax`). A row without the band thresholds keeps the whole span.
    fn band_range(row: &Value, band: usize, lo: i64, hi: i64) -> (i64, i64) {
        const BOUNDS: [&str; 6] = [
            "veryEasyMin",
            "easyMin",
            "normalMin",
            "hardMin",
            "veryHardMin",
            "veryHardMax",
        ];
        let band = band.min(4);
        let (Some(min), Some(next)) = (
            row.get(BOUNDS[band]).and_then(Value::as_i64),
            row.get(BOUNDS[band + 1]).and_then(Value::as_i64),
        ) else {
            return (lo, hi);
        };
        let max = if band == 4 { next } else { next - 1 };
        (min, max.max(min))
    }

    /// Roll a single job Value for a pool + slot. Deterministic via the seed.
    fn roll_job(
        pools_def: &Value,
        pool: &Value,
        character_id: Uuid,
        level: u16,
        reset_boundary: u64,
        slot: u64,
        cycle_pos: i64,
    ) -> Value {
        let pool_id = get_str(pool, "jobPoolId").unwrap_or("");
        let presentation = get_i64(pool, "presentation", 0);
        let base_seed = seed_for(character_id, reset_boundary, pool_id, slot);
        let mut rng = Rng::new(base_seed);

        // The weekly boss pool is always a Duel (type 5); otherwise pick from the
        // non-duel types {0,1,3,4} (Clear=2 unseen in captures → skip).
        let job_type: i64 = if presentation == 2 {
            5
        } else {
            *rng.pick(&[0i64, 1, 3, 4]).unwrap_or(&0)
        };

        let quest_id = uuid_from_seed(base_seed.wrapping_add(0xA11CE));
        let seed_field: i64 = rng.next_u64() as i64; // signed, matches captured range
        let initial_epl = job_initial_epl(pools_def, level);
        let band = cycle_band(pools_def, cycle_pos);
        let difficulty = roll_difficulty(pools_def, job_type, initial_epl, band, &mut rng);

        let mut objectives = serde_json::Map::new();
        for oid in objective_ids(job_type) {
            objectives.insert(
                (*oid).to_string(),
                json!({ "status": "Active", "progress": 0.0, "completed": false }),
            );
        }

        // Still exactly one draw whichever pool it is, so no later roll moves.
        let dungeon = rng.pick(dungeon_pool(job_type)).copied().unwrap_or("");
        let prim_fam = rng.pick(ENEMY_FAMILIES).copied().unwrap_or("");
        let sec_fam = rng.pick(ENEMY_FAMILIES).copied().unwrap_or("");
        let boss_fam = rng.pick(ENEMY_FAMILIES).copied().unwrap_or(prim_fam);
        // A Defeat names its primary enemy and an Explore names its dungeon kit, and
        // both pools above hold ids retail never used there; no non-duel job is ever
        // in an arena (see `nameable`). Swap such a draw, AFTER the draws so no other
        // value on the board moves.
        let (dungeon, prim_fam) = nameable(job_type, dungeon, prim_fam, base_seed);

        let primary_count = if job_type == 5 {
            0
        } else {
            rng.range_incl(3, 6)
        };
        let secondary_count = if job_type == 5 {
            0
        } else {
            rng.range_incl(2, 4)
        };
        let boss_level_delta = rng.range_incl(4, 8);
        let secret_room = job_type != 5 && rng.below(2) == 1;
        // The families above are drawn for their place in the stream only; a
        // non-Duel job takes retail's families from a side stream (#337). A Duel
        // keeps its draw: there is no retail evidence about its family slot.
        let (prim_fam, sec_fam, boss_fam, secret_boss_fam) = if job_type == 5 {
            (prim_fam, sec_fam, boss_fam, boss_fam)
        } else {
            let (p, s, b, sb) = job_families(
                base_seed,
                difficulty,
                difficulty + boss_level_delta,
                difficulty + boss_level_delta + 2,
            );
            (nameable(job_type, dungeon, p, base_seed).1, s, b, sb)
        };
        // Reward curve, fitted to 802 distinct retail jobs mined out of 200
        // captured `/quests` bodies. See `report92_reward_curve` for the fit, the
        // spread, and the negative result that stopped it being exact.
        let d = difficulty.max(1) as u64;
        let reward_xp = job_reward_xp(difficulty); // #337: retail's table, exact
        let _retired_xp_jitter = rng.below(XP_JITTER);
        // A featured or boss job pays a flat gem count INSTEAD of gold; a standard
        // job pays gold and never gems (tracker #306, `report_306_job_gems`). The
        // draws are the retired roll's, in the same order and under the same
        // conditions, so nothing rolled after them (name, gather item, duel boss)
        // moves. A standard job whose retired zero-gold draw hit takes its jitter
        // from a side stream for the same reason.
        let retired_zero_gold = rng.below(1000) < ZERO_GOLD_PER_MILLE;
        let gold_jitter = if retired_zero_gold {
            Rng::new(base_seed ^ GOLD_SIDE_STREAM).below(GOLD_JITTER)
        } else {
            rng.below(GOLD_JITTER)
        };
        if secret_room && rng.below(3) == 0 {
            rng.range_incl(6, 15);
        }
        let reward_gem = job_gem_reward(presentation);
        let reward_item_count = if reward_gem > 0 {
            0
        } else {
            (d * GOLD_PER_DIFFICULTY + GOLD_BASE + gold_jitter) / 10 * 10
        };
        let initial_epl = initial_epl.max(1) as u64;

        let name_idx = rng.below(name_variant_count(job_type)) + 1;
        let name_key = format!("UI.Jobs.Names.{}.{:03}", name_prefix(job_type), name_idx);
        let desc_key = format!("UI.Jobs.Description.{}", name_prefix(job_type));

        let mut job_setup = serde_json::Map::new();
        job_setup.insert("jobType".into(), json!(job_type));
        job_setup.insert("jobCreatorVersion".into(), json!(0));
        job_setup.insert("algorithmVersion".into(), json!(3));
        job_setup.insert("dungeonTemplateId".into(), json!(dungeon));
        if job_type != 5 {
            job_setup.insert("primaryEnemyFamilyId".into(), json!(prim_fam));
            job_setup.insert("secondaryEnemyFamilyId".into(), json!(sec_fam));
        }
        job_setup.insert("bossEnemyFamilyId".into(), json!(boss_fam));
        job_setup.insert("primaryEnemyCount".into(), json!(primary_count));
        job_setup.insert("secondaryEnemyCount".into(), json!(secondary_count));
        job_setup.insert("secondaryEnemyCountPerSpawnerMin".into(), json!(1));
        job_setup.insert("secondaryEnemyCountPerSpawnerMax".into(), json!(1));
        job_setup.insert("enemyBaseLevelOffset".into(), json!(0));
        job_setup.insert("bossLevelDelta".into(), json!(boss_level_delta));
        job_setup.insert("secretRoom".into(), json!(secret_room));
        if secret_room {
            job_setup.insert("secretBossLevelDelta".into(), json!(boss_level_delta + 2));
            job_setup.insert("secretBossEnemyFamilyId".into(), json!(secret_boss_fam));
        }
        job_setup.insert("rewardGemCount".into(), json!(reward_gem));
        job_setup.insert("rewardItemId".into(), json!(REWARD_ITEM_GOLD));
        job_setup.insert("rewardItemCount".into(), json!(reward_item_count));
        job_setup.insert("rewardXp".into(), json!(reward_xp));
        match job_type {
            0 => {
                job_setup.insert("defeatEnemyCount".into(), json!(primary_count));
            }
            3 => {
                job_setup.insert("rescueNpcCount".into(), json!(2));
            }
            4 => {
                let gather = rng.pick(GATHER_ITEMS).copied().unwrap_or("");
                job_setup.insert("gatherItemId".into(), json!(gather));
                job_setup.insert("gatherItemCount".into(), json!(rng.range_incl(3, 6)));
            }
            5 => {
                let duel = rng.pick(DUEL_BOSSES).copied().unwrap_or("");
                job_setup.insert("duelBossId".into(), json!(duel));
            }
            _ => {}
        }
        job_setup.insert("initialEPL".into(), json!(initial_epl));
        // Drawn LAST so the rolls above keep the values they had before elements
        // existed — an extra rng draw earlier would silently reshuffle every job.
        let (name_elems, desc_elems) = dynamic_elements(&mut rng, job_type, &job_setup);
        if name_elems.is_empty() {
            log::error!(
                "[jobs] {name_key} rolled with no name elements (jobType {job_type}, \
                 dungeon {dungeon}, family {prim_fam}) -- the client cannot format it \
                 and the quest map will spin"
            );
        }
        job_setup.insert(
            "questName".into(),
            json!({ "key": name_key, "dynamicElements": name_elems }),
        );
        job_setup.insert(
            "questDescription".into(),
            json!({ "key": desc_key, "dynamicElements": desc_elems }),
        );

        json!({
            "questId": quest_id.to_string(),
            "version": 0,
            "type": "JOB",
            "objectiveStatuses": Value::Object(objectives),
            "difficultyLevel": difficulty,
            "seed": seed_field,
            "jobPoolId": pool_id,
            "jobSetup": Value::Object(job_setup),
            "completed": false,
        })
    }

    /// Generate the full jobs board + pool timers for a character at `now`.
    /// Returns `(jobs, jobPools)`. A malformed/Null `pools_def` yields empty lists.
    pub fn generate(
        pools_def: &Value,
        character_id: Uuid,
        level: u16,
        cycle_index: i64,
        reset_boundary: u64,
        now: u64,
    ) -> (Vec<Value>, Value) {
        let pools = match pools_def.get("jobPools").and_then(|p| p.as_array()) {
            Some(p) => p,
            None => return (Vec::new(), json!([])),
        };
        let max_active_global = pools_def
            .get("globals")
            .and_then(|g| g.get("maxActiveJobs"))
            .and_then(|v| v.as_u64())
            .unwrap_or(4);

        let mut jobs = Vec::new();
        let mut timers = Vec::new();
        // Board position of the pool's first slot: job k on the board takes
        // difficulty-cycle entry `cycle_index + k`.
        let mut board_pos = 0i64;
        for pool in pools {
            let pool_id = match get_str(pool, "jobPoolId") {
                Some(id) => id,
                None => continue,
            };
            let mut count = pool_active_count(pool, reset_boundary);
            if get_i64(pool, "presentation", 0) == 0 {
                count = count.min(max_active_global);
            }
            for slot in 0..count {
                jobs.push(roll_job(
                    pools_def,
                    pool,
                    character_id,
                    level,
                    reset_boundary,
                    slot,
                    cycle_index + board_pos + slot as i64,
                ));
            }
            board_pos += count as i64;
            let (end_time, next_start) = pool_timers(pool, now, count);
            timers.push(json!({ "id": pool_id, "endTime": end_time, "nextStartTime": next_start }));
        }
        (jobs, Value::Array(timers))
    }

    /// Generate the current board while replacing jobs already completed in this
    /// reset window.
    ///
    /// Retail replenishes a town job immediately on `/complete`: the response names
    /// the finished quest in `deletedQuestIds`, returns a full `jobs[]` board of the
    /// same size, and includes generated data only for the new replacement. The
    /// completed row we keep in `quests` is the window-local memory that keeps the
    /// replacement stable until the next reset prunes old job rows.
    ///
    /// Only up to the pool's `maxTotal`, though (tracker #306). In the 2026-06-07
    /// snapshot all 19 completed standard-pool jobs (`maxTotal` 100) were replaced
    /// in the `/complete` response, and none of the 6 completed featured or boss
    /// jobs (`maxTotal` 1) were: the board shrank by one, no generated data was
    /// sent, and every later `/quests` in that window still had no job from the
    /// pool. Refilling the featured slot is what gave HauDrauf a second
    /// "highlighted" job on 2026-10-01, the one that could not be started.
    pub fn generate_replenished(
        pools_def: &Value,
        character_id: Uuid,
        level: u16,
        cycle_index: i64,
        reset_boundary: u64,
        now: u64,
        completed_job_ids: &std::collections::HashSet<Uuid>,
    ) -> (Vec<Value>, Value) {
        if completed_job_ids.is_empty() {
            return generate(
                pools_def,
                character_id,
                level,
                cycle_index,
                reset_boundary,
                now,
            );
        }

        let pools = match pools_def.get("jobPools").and_then(|p| p.as_array()) {
            Some(p) => p,
            None => return (Vec::new(), json!([])),
        };
        let max_active_global = pools_def
            .get("globals")
            .and_then(|g| g.get("maxActiveJobs"))
            .and_then(|v| v.as_u64())
            .unwrap_or(4);

        let mut jobs = Vec::new();
        let mut timers = Vec::new();
        let mut board_pos = 0i64;
        for pool in pools {
            let pool_id = match get_str(pool, "jobPoolId") {
                Some(id) => id,
                None => continue,
            };
            let mut count = pool_active_count(pool, reset_boundary);
            if get_i64(pool, "presentation", 0) == 0 {
                count = count.min(max_active_global);
            }

            // Completed jobs count against `maxTotal` too; a missing value is no cap.
            let max_total = get_u64(pool, "maxTotal", u64::MAX);
            let mut kept = 0;
            let mut issued = 0;
            let mut slot = 0;
            while kept < count && issued < max_total {
                // Same cycle positions as `generate`, so a kept job is unchanged.
                let cycle_pos = cycle_index + board_pos + slot as i64;
                let job = roll_job(
                    pools_def,
                    pool,
                    character_id,
                    level,
                    reset_boundary,
                    slot,
                    cycle_pos,
                );
                slot += 1;
                let Some(id) = get_str(&job, "questId").and_then(|s| Uuid::parse_str(s).ok())
                else {
                    continue;
                };
                issued += 1;
                if completed_job_ids.contains(&id) {
                    continue;
                }
                jobs.push(job);
                kept += 1;
            }
            board_pos += count as i64;

            let (end_time, next_start) = pool_timers(pool, now, count);
            timers.push(json!({ "id": pool_id, "endTime": end_time, "nextStartTime": next_start }));
        }
        (jobs, Value::Array(timers))
    }
}

// ---------------------------------------------------------------------------
// Event ("Sigil") quests (`event_quests`)
// ---------------------------------------------------------------------------
//
// The `/quests` response's `gameEventQuests[]` array is where a timed event quest
// reaches the player. It is NOT a place for ordinary quests: across the retail
// corpus every one of the 2 753 entries in that array carried `type: "GAME_EVENT"`,
// a `gameEventQuestData.gameEventInstanceId`, five milestone `rewards` and a
// `finalReward`. (Between this change and the previous one, we were filling it with
// `type: "NORMAL"` quests picked by a guessed daily rotation — visible in our own
// captured traffic by its tell-tale `seed: 1234`.)
//
// The model, entirely from `game_events.json` + `event_quests.json`:
//
//   * An event repeats every `recurrence.recurrenceInterval` days and each instance
//     stays open `durationSecs` (39 days / 2 days for all 39 events).
//   * While an instance is open, each character gets ONE quest row for it. The row's
//     `questId` is a per-character INSTANCE id; its `gldQuestId` is the template.
//     **Everything downstream must resolve through `gldQuestId`** — the objectives,
//     the dungeon, the rewards and the version all live under the template, and the
//     instance id resolves to nothing at all in `parsed.json`.
//   * Completing the instance is repeatable: the Nth completion pays the Nth
//     milestone, and the last also pays `finalReward` (see `complete_quest`).
pub(crate) mod event_quests {
    use super::*;
    use blades_lib::features::game_events::{self, EventDef, WARNING_LEAD_SECS};
    use blades_lib::game_data::GameData;
    use blades_lib::static_data::StaticData;
    use blades_lib::user_data::{DungeonGeneratedData, GameEventQuestData, QuestType};

    /// One event-quest instance built for a character.
    pub struct MintedEventQuest {
        pub quest_id: Uuid,
        pub quest: blades_lib::user_data::Quest,
        pub dungeon: Option<DungeonGeneratedData>,
    }

    /// The per-character instance quest id for an event instance.
    ///
    /// Deterministic so that re-fetching `/quests` inside the window resolves the
    /// same stored row — otherwise every poll would mint a new quest and the player's
    /// objective progress and milestone count would reset under them. Derived from
    /// `(character_id, gameEventInstanceId)`, which already contains the event id and
    /// the window start, so two windows of the same event get different ids.
    pub fn instance_quest_id(character_id: Uuid, game_event_instance_id: &str) -> Uuid {
        let mut h: u64 = 0xCBF2_9CE4_8422_2325; // FNV-1a
        let mut lo: u64 = 0x84222325_CBF29CE4;
        for b in character_id
            .as_bytes()
            .iter()
            .chain(game_event_instance_id.as_bytes())
        {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
            lo = lo.rotate_left(7) ^ h;
        }
        let mut bytes = [0u8; 16];
        bytes[0..8].copy_from_slice(&h.to_le_bytes());
        bytes[8..16].copy_from_slice(&lo.to_le_bytes());
        bytes[6] = (bytes[6] & 0x0F) | 0x40; // v4 shape
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        Uuid::from_bytes(bytes)
    }

    /// Build the wire body for one event instance. `None` when the template quest is
    /// not in `parsed.json` (then we cannot honestly produce objectives or a dungeon,
    /// so the event is simply not advertised rather than advertised unplayable).
    fn build(
        def: &EventDef,
        instance_start: i64,
        static_data: &StaticData,
        game_data: &GameData,
        character_id: Uuid,
        player_level: i64,
    ) -> Option<MintedEventQuest> {
        let instance_id = format!("{}::{}", def.event_id, instance_start);
        let quest_id = instance_quest_id(character_id, &instance_id);

        // Resolve the body through the TEMPLATE id — `def.quest_id` is the gldQuestId.
        let (mut quest, mut dungeon) = generate_quest_data(
            game_data,
            def.quest_id,
            player_level,
            &static_data.quests_daily.level_scaling,
        )
        .ok()?;
        if dungeon.is_some() {
            let dungeon_id = game_data
                .quests
                .get(&def.quest_id)?
                .dungeon_info
                .as_ref()?
                .dungeon_uuid;
            let enemy_level = static_data
                .quests_daily
                .level_scaling
                .enemy_level(player_level);
            // Every stage of the event's dungeon (#323) and no other (#329), seeded
            // per run.
            dungeon = blades_lib::util::quest::generate_for_event_dungeon_with_seed(
                game_data,
                &dungeon_id,
                blades_lib::util::dungeon::run_loot_seed(&quest_id, 0),
                enemy_level,
                &static_data.quests_daily.level_scaling,
            );
        }

        quest.r#type = QuestType::GameEvent;
        quest.gld_quest_id = def.quest_id;
        quest.game_event_quest_data = Some(GameEventQuestData {
            game_event_instance_id: instance_id,
        });
        // The ladder of the character's level band, fixed for the instance's
        // lifetime as retail fixed it (report #333).
        if let Some(tmpl) = static_data.event_quests.templates.get(&def.quest_id) {
            quest.rewards = Some(tmpl.rewards_for_level(player_level));
            quest.final_reward = tmpl.final_reward_for_level(player_level);
        }
        Some(MintedEventQuest {
            quest_id,
            quest,
            dungeon,
        })
    }

    /// The event quests whose instance window covers `now`.
    pub fn mint(
        static_data: &StaticData,
        game_data: &GameData,
        character_id: Uuid,
        player_level: i64,
        now: i64,
    ) -> Vec<MintedEventQuest> {
        // Through `open_instances`, NOT `active_instance_start` per def: that is
        // the shared, capped answer the `/gameevents` feed also uses. Computing it
        // here independently is how a third event quest reached a client whose
        // quest screen retail never gave three.
        // …and with the themed window, for the same reason: a window the feed
        // honoured and the quest rows ignored would advertise an event nobody
        // could play.
        let theme = static_data.game_event_theme.as_ref();
        game_events::open_instances_themed(&static_data.game_events, theme, now)
            .into_iter()
            .filter_map(|(start, def)| {
                build(
                    def,
                    start,
                    static_data,
                    game_data,
                    character_id,
                    player_level,
                )
            })
            .collect()
    }

    /// The event quests whose window opens within the warning lead (24 h).
    pub fn upcoming(
        static_data: &StaticData,
        game_data: &GameData,
        character_id: Uuid,
        player_level: i64,
        now: i64,
    ) -> Vec<QuestWithId> {
        let theme = static_data.game_event_theme.as_ref();
        game_events::upcoming_events_themed(&static_data.game_events, theme, now, WARNING_LEAD_SECS)
            .into_iter()
            .filter_map(|e| {
                // By the instance id, not the quest: it names the exact def the
                // feed chose, so the row's `gameEventInstanceId` matches it.
                let def = static_data.game_events.iter().find(|d| {
                    e.game_event_instance_id == format!("{}::{}", d.event_id, e.start_time_secs)
                })?;
                let m = build(
                    def,
                    e.start_time_secs,
                    static_data,
                    game_data,
                    character_id,
                    player_level,
                )?;
                Some(QuestWithId {
                    quest_id: m.quest_id,
                    quest: m.quest,
                })
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod jobs_tests_support {
    use serde_json::Value;
    pub fn sample_pools_for_diff() -> Value {
        super::jobs_tests::sample_pools()
    }
}

#[cfg(test)]
mod jobs_tests {
    use super::jobs_gen;
    use serde_json::{Value, json};
    use uuid::Uuid;

    pub(crate) fn sample_pools() -> Value {
        // A trimmed but shape-faithful job_pools.json: one standard/daily pool
        // (maxActive 4), one boss/special weekly, and one featured weekly on Sun.
        json!({
            "globals": {
                "maxActiveJobs": 4,
                "dailyJobsRefreshTimeHour": 5,
                "dailyJobsRefreshTimeMinute": 0,
                "difficultyCycle": [0,1,2,1,0,3,0,1],
                "baseJobDifficultyRange": { "min": -2, "max": 9 }
            },
            "jobPools": [
                { "jobPoolId": "4956c6ab-1832-4edd-8bee-561b79f83ee2", "presentation": 0,
                  "maxActive": 4, "recurrence": { "type": 1, "resetHour": 5, "resetMinute": 0, "dayOfWeek": 0 } },
                { "jobPoolId": "361da91e-6860-4c31-a447-4010cbaad1dd", "presentation": 2,
                  "maxActive": 1, "recurrence": { "type": 2, "resetHour": 5, "resetMinute": 0, "dayOfWeek": 0 } },
                { "jobPoolId": "9d94baeb-96d4-49e9-bdf6-9f939be836d3", "presentation": 1,
                  "maxActive": 1, "recurrence": { "type": 2, "resetHour": 5, "resetMinute": 0, "dayOfWeek": 6 } }
            ],
            "perTypeDifficulty": [
                { "jobType": 0, "difficultyByLevel": [
                    { "level": 1, "veryEasyMin": -2, "veryHardMax": 9 },
                    { "level": 20, "veryEasyMin": -4, "veryHardMax": 7 }
                ]}
            ]
        })
    }

    // 2026-05-13 06:00 UTC (Wednesday, after the 05:00 reset).
    const NOW_WED: u64 = 1_778_648_400 + 3600;
    const CHAR: Uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);

    #[test]
    fn jobs_generated_non_empty_from_sample() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (jobs, _timers) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        // 4 standard daily + 1 weekly boss (Wed is not the featured pool's day).
        assert!(!jobs.is_empty(), "board must not be empty");
        assert_eq!(
            jobs.len(),
            5,
            "4 daily + 1 boss expected, got {}",
            jobs.len()
        );
    }

    #[test]
    fn every_job_has_type_job_and_populated_job_setup() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (jobs, _t) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        for j in &jobs {
            assert_eq!(j["type"], "JOB");
            assert!(j["questId"].as_str().is_some(), "questId present");
            assert!(
                Uuid::parse_str(j["questId"].as_str().unwrap()).is_ok(),
                "questId is a UUID"
            );
            let js = &j["jobSetup"];
            assert!(js.is_object(), "jobSetup present");
            // Required jobSetup fields are populated (non-null).
            for key in [
                "jobType",
                "jobCreatorVersion",
                "algorithmVersion",
                "dungeonTemplateId",
                "bossEnemyFamilyId",
                "primaryEnemyCount",
                "secondaryEnemyCount",
                "enemyBaseLevelOffset",
                "bossLevelDelta",
                "secretRoom",
                "rewardGemCount",
                "rewardItemId",
                "rewardItemCount",
                "rewardXp",
                "initialEPL",
                "questName",
            ] {
                assert!(!js[key].is_null(), "jobSetup.{key} populated");
            }
            assert!(
                js["dungeonTemplateId"].as_str().unwrap().len() == 36,
                "real dungeon id"
            );
            assert!(
                j["difficultyLevel"].as_i64().unwrap() >= 1,
                "difficulty >= 1"
            );
            assert!(
                j["objectiveStatuses"].as_object().unwrap().len() >= 1,
                "has objectives"
            );
        }
    }

    #[test]
    fn duel_boss_job_has_duel_fields() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (jobs, _t) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        let boss = jobs
            .iter()
            .find(|j| j["jobPoolId"] == "361da91e-6860-4c31-a447-4010cbaad1dd")
            .expect("boss pool produced a job");
        assert_eq!(boss["jobSetup"]["jobType"], 5, "boss pool -> Duel");
        assert!(
            boss["jobSetup"]["duelBossId"].as_str().is_some(),
            "duelBossId present"
        );
    }

    #[test]
    fn timers_are_relative_to_now_not_frozen() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (_j, timers) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        let arr = timers.as_array().expect("timers array");
        assert_eq!(arr.len(), 3, "one timer per pool");
        // The daily pool's next reset must be strictly in the future and within 24h.
        let daily = arr
            .iter()
            .find(|p| p["id"] == "4956c6ab-1832-4edd-8bee-561b79f83ee2")
            .unwrap();
        let end = daily["endTime"].as_u64().unwrap();
        assert!(end > NOW_WED, "daily endTime is in the future");
        assert!(end - NOW_WED <= 86_400, "daily endTime within a day");
        // Every non-zero timer is strictly after now (no stale 2026-03 constants).
        for p in arr {
            for k in ["endTime", "nextStartTime"] {
                let t = p[k].as_u64().unwrap();
                assert!(
                    t == 0 || t > NOW_WED,
                    "{} {} must be 0 or > now (got {})",
                    p["id"],
                    k,
                    t
                );
            }
        }
    }

    #[test]
    fn deterministic_same_window_same_board() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let a = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        // A later `now` in the SAME window (same reset boundary) → same jobs.
        let b = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED + 3600);
        assert_eq!(a.0, b.0, "same reset window regenerates identical jobs");
    }

    #[test]
    fn different_character_different_board() {
        let pools = sample_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let other = Uuid::from_u128(0xdead_beef_dead_beef_dead_beef_dead_beef);
        let a = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);
        let b = jobs_gen::generate(&pools, other, 30, 0, boundary, NOW_WED);
        assert_ne!(a.0, b.0, "different characters roll different jobs");
    }

    #[test]
    fn null_pools_degrade_to_empty() {
        let (jobs, timers) = jobs_gen::generate(&Value::Null, CHAR, 30, 0, 0, NOW_WED);
        assert!(jobs.is_empty(), "Null pools -> empty jobs, no panic");
        assert_eq!(timers, json!([]), "Null pools -> empty timers");
        // A malformed shape (jobPools not an array) also degrades.
        let bad = json!({ "jobPools": 42, "globals": "nope" });
        let (jobs2, _t2) = jobs_gen::generate(&bad, CHAR, 30, 0, 0, NOW_WED);
        assert!(jobs2.is_empty(), "malformed pools -> empty jobs");
    }

    #[test]
    fn featured_pool_active_only_on_its_day() {
        let pools = sample_pools();
        // Sunday after reset: the featured pool with dayOfWeek=6 (Sat) is active
        // during the Sun game-day (window opened Sat 05:00). 2026-05-17 is a Sunday.
        let now_sun = 1_779_080_400 - 86_400 + 3600; // Sun 06:00 UTC (approx via boss timer base)
        let boundary = jobs_gen::current_reset_boundary(&pools, now_sun);
        let (_j, timers) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, now_sun);
        // The board is at least the daily 4 + boss 1 on any day.
        let (jobs_wed, _) = {
            let b = jobs_gen::current_reset_boundary(&pools, NOW_WED);
            jobs_gen::generate(&pools, CHAR, 30, 0, b, NOW_WED)
        };
        assert!(jobs_wed.len() >= 5, "daily+boss always present");
        // timers array always lists all 3 pools regardless of activity.
        assert_eq!(timers.as_array().unwrap().len(), 3);
    }

    /// End-to-end against the *real* committed `job_pools.json` (the faithful APK
    /// extract the live server loads). Guards that the shipped data still drives a
    /// non-empty, prod-shaped board. NOW_WED is 2026-05-13 Wed 06:00 UTC (after the
    /// 05:00 reset) — the day matching prod capture id=1105, whose board was:
    ///   4 daily (`4956c6ab`) + 1 weekly boss (`361da91e`) + 1 featured (`9fcbb01c`,
    ///   dayOfWeek=2/Tue, active during the Wed game-day) = 6 jobs, and 10 timers.
    /// The job board never grows past the size retail ever sent, on ANY day.
    ///
    /// MEASURED over 773 captured `/quests` bodies: `jobs` holds 4, 5 or 6 entries
    /// and never more. The test above pins one Wednesday at 6; this sweeps a whole
    /// year, because the pools that make up the board are not all on the same
    /// cycle — four standard daily, one featured daily, one featured per weekday,
    /// and a boss pool whose instance runs a full week. Nothing in `generate`
    /// bounds their SUM: `maxActiveJobs` is applied per pool, and only to pools
    /// with `presentation == 0`.
    ///
    /// This is the same shape of hole that left the quest map spinning: the client
    /// builds that screen from the boot `/quests` response and never re-requests
    /// it, so an array longer than retail ever sent wedges the screen until the app
    /// restarts. If a future pool edit pushes some weekday to 7, this fails here
    /// instead of on a player's device.
    #[test]
    fn no_day_of_the_year_produces_a_longer_board_than_retail_ever_sent() {
        /// Max `jobs[]` length across the corpus: 6 (635 bodies), 5 (82), 4 (56).
        const RETAIL_MAX_JOBS: usize = 6;
        const DAY: u64 = 86_400;

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/job_pools.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let pools: Value = serde_json::from_str(&raw).expect("valid job_pools.json");

        let mut seen: std::collections::BTreeMap<usize, usize> = Default::default();
        for day in 0..365u64 {
            let now = NOW_WED + day * DAY;
            let boundary = jobs_gen::current_reset_boundary(&pools, now);
            let (jobs, _) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, now);
            *seen.entry(jobs.len()).or_default() += 1;
            assert!(
                jobs.len() <= RETAIL_MAX_JOBS,
                "day +{day}: board of {} jobs, retail never sent more than {RETAIL_MAX_JOBS}",
                jobs.len()
            );
        }

        // Control: a generator that returned nothing would satisfy the bound above
        // without telling us anything, and so would one stuck on a single count.
        assert!(
            seen.keys().copied().all(|n| n > 0),
            "some day produced an empty board: {seen:?}"
        );
        assert!(
            seen.contains_key(&RETAIL_MAX_JOBS),
            "the sweep never reached a full {RETAIL_MAX_JOBS}-job board, so the \
             bound was never approached: {seen:?}"
        );
    }

    #[test]
    fn real_job_pools_file_generates_prod_shaped_board() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/job_pools.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let pools: Value = serde_json::from_str(&raw).expect("valid job_pools.json");

        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (jobs, timers) = jobs_gen::generate(&pools, CHAR, 30, 0, boundary, NOW_WED);

        assert_eq!(
            timers.as_array().unwrap().len(),
            10,
            "10 pools -> 10 timers"
        );
        assert_eq!(
            jobs.len(),
            6,
            "Wed board = 4 daily + 1 boss + 1 featured, got {}",
            jobs.len()
        );
        // The featured pool active on this weekday produced exactly one job.
        assert_eq!(
            jobs.iter()
                .filter(|j| j["jobPoolId"] == "9fcbb01c-13bf-4cd9-916f-25d5faf5314e")
                .count(),
            1,
            "Tue-featured pool active during the Wed game-day"
        );

        // Every timer is 0 or strictly in the future (no frozen 2026-03 constants).
        for p in timers.as_array().unwrap() {
            for k in ["endTime", "nextStartTime"] {
                let t = p[k].as_u64().unwrap();
                assert!(
                    t == 0 || t > NOW_WED,
                    "timer {} {} stale: {}",
                    p["id"],
                    k,
                    t
                );
            }
        }
        // The daily standard pool produced exactly maxActiveJobs (4) entries.
        let daily = jobs
            .iter()
            .filter(|j| j["jobPoolId"] == "4956c6ab-1832-4edd-8bee-561b79f83ee2")
            .count();
        assert_eq!(daily, 4, "standard daily pool -> maxActiveJobs (4)");
        // The boss pool produced a Duel with a duelBossId from the real data.
        let boss = jobs
            .iter()
            .find(|j| j["jobPoolId"] == "361da91e-6860-4c31-a447-4010cbaad1dd")
            .expect("boss pool job");
        assert_eq!(boss["jobSetup"]["jobType"], 5);
        assert!(boss["jobSetup"]["duelBossId"].as_str().is_some());

        // Every generated job round-trips into a storable Quest row (accept path).
        let gd = super::report85_job_generated_data_tests::game_data();
        for j in &jobs {
            assert!(
                jobs_gen::job_quest_db_entry(j, CHAR, &gd, &crate::quest::shipped_scaling())
                    .is_some(),
                "job must build a persistable quest row"
            );
        }
    }
}

/// Report #92: our job reward numbers were invented; these are fitted.
///
/// The mechanism was fixed first (#180 — pay what the `jobSetup` declares), so what
/// a player receives has always matched what the board advertised. This is about the
/// numbers on the board being the right SIZE.
///
/// MINED: 802 distinct retail jobs out of 200 captured `/quests` bodies. Least
/// squares:
///
/// ```text
/// rewardXp        ~ 18.92 * difficultyLevel + 47.7   (n = 802)
/// rewardItemCount ~ 19.25 * difficultyLevel + 107.2  (n = 654)
/// 148 of 802 jobs (18.5%) pay NO gold at all
/// ```
///
/// XP: SUPERSEDED by #337. The "negative result" once recorded here — two XP
/// populations per `(characterLevel, difficultyLevel)` with a hidden input — was a
/// measurement artefact: the capture DB also holds 641 jobs that OUR server rolled
/// after the shutdown (2026-06-30 onward), and those carried the fitted line plus
/// jitter. Restricted to the 1,443 pre-shutdown retail jobs, `rewardXp` is an exact
/// function of `difficultyLevel` with 0 conflicts, and [`job_reward_xp`] is that
/// table. The gold fit below is unchanged.
#[cfg(test)]
mod report92_reward_curve {
    use super::jobs_gen::*;
    use serde_json::Value;

    fn job_pools() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/job_pools.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("job_pools.json"))
            .expect("valid job_pools.json")
    }

    const CHAR: uuid::Uuid = uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    const NOW_WED: u64 = 1_778_648_400 + 3600;

    /// Roll a lot of boards across many characters and levels, and compare the
    /// resulting cloud against the retail fit.
    fn sample() -> Vec<(i64, u64, u64)> {
        let pools = job_pools();
        let boundary = current_reset_boundary(&pools, NOW_WED);
        let mut out = Vec::new();
        for seed in 0..60u128 {
            let c = uuid::Uuid::from_u128(CHAR.as_u128().wrapping_add(seed));
            for level in [10u16, 30, 50, 70, 90] {
                let (jobs, _t) = generate(&pools, c, level, 0, boundary, NOW_WED);
                for j in jobs {
                    let d = j["difficultyLevel"].as_i64().unwrap_or(0);
                    let xp = j["jobSetup"]["rewardXp"].as_u64().unwrap_or(0);
                    let gold = j["jobSetup"]["rewardItemCount"].as_u64().unwrap_or(0);
                    out.push((d, xp, gold));
                }
            }
        }
        assert!(
            out.len() > 500,
            "the committed pools must roll a real sample"
        );
        out
    }

    /// Every rolled job advertises retail's exact XP for its difficulty (#337) — no
    /// spread. The fitted line plus jitter it replaces paid 466-504 for a
    /// difficulty-21 job; retail paid 376 on all 11 it rolled.
    #[test]
    fn every_rolled_job_pays_retails_xp_for_its_difficulty() {
        let s = sample();
        for (d, xp, _) in &s {
            assert_eq!(*xp, job_reward_xp(*d), "difficulty {d} job advertised {xp} xp");
        }
    }

    /// Gold likewise — and this one moved the other way, from 30 down to 19.
    #[test]
    fn gold_per_difficulty_matches_the_retail_fit() {
        let s = sample();
        let paying: Vec<_> = s.iter().filter(|(d, _, g)| *d > 0 && *g > 0).collect();
        let ratio: f64 = paying
            .iter()
            .map(|(d, _, g)| *g as f64 / *d as f64)
            .sum::<f64>()
            / paying.len() as f64;
        assert!(
            (15.0..=28.0).contains(&ratio),
            "gold/difficulty {ratio:.2} is outside the retail band (fit 19.25);              the old constant gave about 30"
        );
    }

    /// Retail's 148 no-gold jobs (18.5%) were not a random share: they were the
    /// featured and boss jobs, which pay gems instead (#306). In 463 distinct retail
    /// jobs, 111 of 111 gem jobs pay no gold and 352 of 352 standard jobs pay gold.
    #[test]
    fn the_jobs_that_pay_no_gold_are_exactly_the_gem_jobs() {
        let pools = job_pools();
        let boundary = current_reset_boundary(&pools, NOW_WED);
        let (mut gem_jobs, mut gold_jobs) = (0, 0);
        for seed in 0..60u128 {
            let c = uuid::Uuid::from_u128(CHAR.as_u128().wrapping_add(seed));
            for level in [10u16, 30, 50, 70, 90] {
                for j in generate(&pools, c, level, 0, boundary, NOW_WED).0 {
                    let gems = j["jobSetup"]["rewardGemCount"].as_u64().unwrap();
                    let gold = j["jobSetup"]["rewardItemCount"].as_u64().unwrap();
                    assert!(
                        (gems > 0) != (gold > 0),
                        "a job pays gems or gold, never both or neither: {gems} gems, {gold} gold"
                    );
                    if gems > 0 { gem_jobs += 1 } else { gold_jobs += 1 }
                }
            }
        }
        assert!(gem_jobs > 0 && gold_jobs > 0, "the sample holds both kinds");
    }

    /// Gold stays a round ten, as every captured value is.
    #[test]
    fn gold_is_always_a_multiple_of_ten() {
        for (_, _, g) in sample() {
            assert_eq!(
                g % 10,
                0,
                "captured rewardItemCount is always a round ten, got {g}"
            );
        }
    }
}

/// Tracker #337 (Sephoris, level 22): "1000 XP per job today, 2000 yesterday".
///
/// Retail ground truth, 1,443 distinct pre-shutdown jobs from the capture DB:
/// `rewardXp` is a pure function of `difficultyLevel` (0 conflicts). At character
/// level 20-25 retail's 53 jobs sat at median difficulty 22 and paid median 400 XP
/// on `/complete`; with ~11 enemies' kill XP (median 908) a whole job was worth a
/// median 1,313 (range 885-1,847). We paid 456-614 on difficulties 19-27.
#[cfg(test)]
mod report337_job_xp {
    use super::jobs_gen::*;
    use serde_json::Value;

    /// Values read straight off the retail corpus, with how many distinct retail
    /// jobs carried each (every one of them, no other value seen).
    #[test]
    fn job_xp_is_retails_value_for_its_difficulty() {
        for (difficulty, xp, retail_jobs) in [
            (1, 50, 25),
            (19, 332, 10),
            (20, 354, 16),
            (21, 376, 11),
            (22, 400, 11),
            (24, 448, 12),
            (26, 500, 11),
            (27, 528, 15),
            (48, 1200, 6),
            (50, 1276, 9),
            (51, 1286, 19),
            (75, 1526, 45),
            (84, 1616, 2),
        ] {
            assert_eq!(
                job_reward_xp(difficulty),
                xp,
                "difficulty {difficulty}: retail paid {xp} on {retail_jobs} of {retail_jobs} jobs"
            );
        }
        // 83 is the corpus's one gap inside 1..=84; 51..=84 is +10 per level.
        assert_eq!(job_reward_xp(83), 1606);
        // Out of range: clamp low, continue the +10 line high.
        assert_eq!(job_reward_xp(0), 50);
        assert_eq!(job_reward_xp(-3), 50);
        assert_eq!(job_reward_xp(85), 1626);
        assert_eq!(job_reward_xp(100), 1776);
    }

    /// The table only ever climbs — a dip would make a harder job pay less.
    #[test]
    fn harder_jobs_never_pay_less() {
        for d in 1..=110 {
            assert!(job_reward_xp(d + 1) > job_reward_xp(d), "difficulty {d} -> {}", d + 1);
        }
    }

    fn job_pools() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/job_pools.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("job_pools.json"))
            .expect("valid job_pools.json")
    }

    /// The reporter's own 2026-10-04 board (level 20, cycle index 26), which prod
    /// stored as: e4742b8e d21 504xp 590g, f950c41c d26 543xp 640g, 6f6de44d d20
    /// 507xp 490g, 58d85b87 d21 466xp 580g, 684ab17d d23 563xp 12 gems, 6ac9d913
    /// d21 479xp 4 gems. Everything but the XP must come out the same — the XP
    /// draw is still taken, so no later roll moves — and the XP must be retail's.
    #[test]
    fn the_reporters_board_keeps_its_jobs_and_pays_retail_xp() {
        let pools = job_pools();
        let character = uuid::Uuid::parse_str("f7817aa6-5892-4794-871e-9b51a475a606").unwrap();
        let boundary = 1_791_090_000; // 2026-10-04 05:00 UTC
        let (jobs, _) = generate(&pools, character, 20, 26, boundary, boundary + 3600);
        let got: Vec<(String, i64, u64, u64, u64)> = jobs
            .iter()
            .map(|j| {
                let s = &j["jobSetup"];
                (
                    j["questId"].as_str().unwrap()[..8].to_string(),
                    j["difficultyLevel"].as_i64().unwrap(),
                    s["rewardXp"].as_u64().unwrap(),
                    s["rewardItemCount"].as_u64().unwrap(),
                    s["rewardGemCount"].as_u64().unwrap(),
                )
            })
            .collect();
        let want: Vec<(String, i64, u64, u64, u64)> = [
            ("e4742b8e", 21, 376, 590, 0),
            ("f950c41c", 26, 500, 640, 0),
            ("6f6de44d", 20, 354, 490, 0),
            ("58d85b87", 21, 376, 580, 0),
            ("684ab17d", 23, 424, 0, 12),
            ("6ac9d913", 21, 376, 0, 4),
        ]
        .into_iter()
        .map(|(id, d, xp, gold, gems)| (id.to_string(), d, xp, gold, gems))
        .collect();
        assert_eq!(got, want);
    }
}

/// Report #85: the quest map loads forever, and deleting `job_pools.json` "fixes" it.
///
/// Retail sends a `dungeonGeneratedDataList` entry for EVERY job. We sent none, so the
/// client listed the jobs on the map and then waited for spawn data that never came —
/// and with no jobs at all there were no unresolvable ids, which is why removing the
/// pool file made the hang go away while producing an empty board.
///
/// Ground truth is the one committed full `/quests` body,
/// `blades-capture reference/capture-599.jsonl`: 6 jobs, 2 quests, 10 generated-data
/// entries; 6/6 job ids present and — the control — 2/2 quest ids present.
///
/// The tests that shipped this bug (`jobs_tests`) were shape-only: they checked that a
/// job had a `jobSetup` and a UUID, never that anything could resolve it. These pin the
/// invariant instead, and every one of them is paired with a story-quest control so a
/// red run means the job path broke rather than the fixture failing to load.
#[cfg(test)]
mod report85_job_generated_data_tests {
    use super::*;
    use blades_lib::game_data::GameData;
    use blades_lib::static_data::QuestLevelScaling;
    use std::collections::HashSet;

    pub fn game_data() -> GameData {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid parsed.json")
    }

    fn job_pools() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/job_pools.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid job_pools.json")
    }

    const CHAR: Uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    /// 2026-05-13 Wed 06:00 UTC — the weekday whose board prod capture id=1105 shows.
    const NOW_WED: u64 = 1_778_648_400 + 3600;

    /// The real board, rolled from the committed job pools.
    fn board() -> Vec<Value> {
        let pools = job_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (jobs, _t) = jobs_gen::generate(&pools, CHAR, 48, 0, boundary, NOW_WED);
        assert!(
            !jobs.is_empty(),
            "the committed pools must roll a board at all"
        );
        jobs
    }

    /// A story quest that really has a dungeon, used as the control everywhere below.
    /// Picked from `parsed.json` at run time rather than hard-coded so the control cannot
    /// rot into a quest that no longer exists.
    fn story_quest_with_dungeon(gd: &GameData) -> Uuid {
        let mut ids: Vec<Uuid> = gd
            .quests
            .iter()
            .filter(|(_, q)| {
                q.dungeon_info.as_ref().is_some_and(|d| {
                    !d.dungeon_uuid.is_nil() && gd.dungeons.contains_key(&d.dungeon_uuid)
                })
            })
            .map(|(id, _)| *id)
            .collect();
        ids.sort();
        assert!(
            !ids.is_empty(),
            "parsed.json has at least one dungeon-backed quest"
        );
        ids[0]
    }

    fn story_generated(gd: &GameData) -> blades_lib::user_data::DungeonGeneratedData {
        let (_q, data) = generate_quest_data(
            gd,
            story_quest_with_dungeon(gd),
            48,
            &QuestLevelScaling::default(),
        )
        .expect("a dungeon-backed quest generates");
        data.expect("…with dungeon data")
    }

    /// THE bug. Every job the client is told about must have a generated-data entry
    /// keyed by the same id — retail: 6/6.
    ///
    /// Asserted against [`assemble_generated_data_list`], the function the route actually
    /// builds the array with, not against the job helper in isolation: a helper that works
    /// while nothing calls it is precisely the state this repo was in.
    #[test]
    fn every_job_on_the_board_has_generated_data() {
        let gd = game_data();
        let jobs = board();
        let list =
            assemble_generated_data_list(Vec::new(), &gd, &jobs, &crate::quest::shipped_scaling());

        let job_ids: Vec<Uuid> = jobs
            .iter()
            .map(|j| Uuid::parse_str(j["questId"].as_str().expect("questId")).expect("uuid"))
            .collect();
        let with_data: HashSet<Uuid> = list.iter().map(|g| g.quest_id).collect();

        let missing: Vec<Uuid> = job_ids
            .iter()
            .copied()
            .filter(|id| !with_data.contains(id))
            .collect();
        assert!(
            missing.is_empty(),
            "{} of {} jobs have no generatedData entry — the quest map waits forever for \
             them (report #85). Missing: {missing:?}",
            missing.len(),
            job_ids.len(),
        );
        assert_eq!(
            list.len(),
            job_ids.len(),
            "exactly one entry per job, no extras"
        );
    }

    /// The control for the test above. If `parsed.json` failed to load, or
    /// `generate_for_dungeon` broke outright, this goes red too — so a lone failure up
    /// there means the JOB path regressed, not the harness.
    #[test]
    fn story_quests_still_have_generated_data() {
        let gd = game_data();
        let data = story_generated(&gd);
        assert!(
            !data.enemy_generated_data.is_empty(),
            "the control quest must still generate enemies"
        );
    }

    /// An entry the client cannot populate a level from is no better than a missing one:
    /// the 17 `JobCaveVariant_*` ids our roller puts in `jobSetup.dungeonTemplateId` all
    /// exist in `parsed.json` with an EMPTY `spawn_info`, so generating from the template
    /// id yields an entry with no enemies at all. This test is what catches that mistake.
    #[test]
    fn a_jobs_generated_data_is_not_empty() {
        let gd = game_data();
        for job in board() {
            let data =
                jobs_gen::generated_data_for_job(&gd, &job, &crate::quest::shipped_scaling())
                    .expect("the reference dungeon resolves");
            assert!(
                !data.enemy_generated_data.is_empty(),
                "job {} generated no enemies — nothing to kill, nothing to complete",
                job["questId"]
            );
            assert!(!data.chest_generated_data.is_empty(), "…and no chests");
        }
        // The control that makes the above discriminating: generating from the
        // dungeonTemplateId instead — the plausible wrong answer — really is empty.
        let template: Uuid = Uuid::parse_str(
            board()[0]["jobSetup"]["dungeonTemplateId"]
                .as_str()
                .expect("template id"),
        )
        .expect("uuid");
        let from_template =
            blades_lib::util::dungeon::generate_for_dungeon(&gd, &template, 40, 4000)
                .expect("the template dungeon exists in parsed.json");
        assert!(
            from_template.enemy_generated_data.is_empty(),
            "if the template id ever gains spawn info, revisit JOB_SPAWN_GROUPS_REFERENCE"
        );
    }

    /// Tracker #121's Duel hung before the scene started. The one committed
    /// retail Duel is an exact discriminator: it carries three one-enemy groups,
    /// no item groups, and two chest groups, while non-Duel jobs beside it carry
    /// primary/secondary packs and item rolls. We used to emit the entire shared
    /// reference (six groups / fourteen enemies / all items) for every type.
    #[test]
    fn a_duel_uses_retails_duel_generated_data_shape() {
        let gd = game_data();
        let jobs = board();
        let duel = jobs
            .iter()
            .find(|j| j["jobSetup"]["jobType"] == 5)
            .expect("the weekly boss pool must produce a Duel");
        let data = jobs_gen::generated_data_for_job(&gd, duel, &crate::quest::shipped_scaling())
            .expect("Duel generates");

        let actual: HashSet<Uuid> = data.enemy_generated_data.keys().copied().collect();
        let expected: HashSet<Uuid> = jobs_gen::DUEL_ENEMY_SPAWN_GROUPS.into_iter().collect();
        assert_eq!(actual, expected, "Duel must not receive ordinary job packs");
        assert_eq!(
            data.enemy_generated_data
                .values()
                .flat_map(|spawners| spawners.iter())
                .map(Vec::len)
                .sum::<usize>(),
            3,
            "retail generated exactly three Duel enemies"
        );
        assert!(
            data.item_generated_data.is_empty(),
            "retail Duel has no floor-item data"
        );
        assert_eq!(
            data.chest_generated_data.len(),
            2,
            "retail Duel keeps both chest groups"
        );

        let difficulty = duel["difficultyLevel"].as_i64().unwrap();
        let boss_delta = duel["jobSetup"]["bossLevelDelta"].as_i64().unwrap();
        for (spawn_id, spawners) in &data.enemy_generated_data {
            let expected_level = if *spawn_id == jobs_gen::JOB_BOSS_SPAWN_GROUP {
                difficulty + boss_delta
            } else {
                difficulty
            };
            assert!(
                spawners
                    .iter()
                    .flatten()
                    .all(|enemy| enemy.enemy_level == expected_level),
                "spawn {spawn_id} has the wrong Duel level"
            );
        }

        // Discriminating control: the non-Duel path still has ordinary packs and
        // item data, so the repair was not a blanket truncation of all jobs.
        let ordinary = jobs
            .iter()
            .find(|j| j["jobSetup"]["jobType"] != 5)
            .expect("the daily pool must produce ordinary jobs");
        let ordinary_data =
            jobs_gen::generated_data_for_job(&gd, ordinary, &crate::quest::shipped_scaling())
                .expect("ordinary job generates");
        assert!(ordinary_data.enemy_generated_data.len() > 3);
        assert!(!ordinary_data.item_generated_data.is_empty());
    }

    /// EVERY HARDCODED SPAWN-GROUP UUID MUST MATCH THE SHIPPED DATA.
    ///
    /// These ids are transcribed by hand from `job_pools.json`, and one of them was
    /// got wrong exactly that way: `d00e3919-…`'s last nine hex digits were filled
    /// in from a truncated terminal dump rather than read from the file. Five of six
    /// happened to be right. A wrong id here is silent — the `remove` simply matches
    /// nothing, the group is still over-sent, and every test about SHAPE still
    /// passes because the count is only checked against the rules, never the source.
    ///
    /// So the file is the authority, and this asserts against it directly.
    #[test]
    fn the_hardcoded_spawn_group_ids_match_job_pools_json() {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/job_pools.json");
        let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        let pools: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let g = &pools["globals"];

        let by_rarity = |key: &str, rarity: i64| -> Uuid {
            g[key]
                .as_array()
                .unwrap_or_else(|| panic!("{key} is a list"))
                .iter()
                .find(|e| e["rarity"].as_i64() == Some(rarity))
                .and_then(|e| e["spawnGroupId"].as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or_else(|| panic!("{key} rarity {rarity} missing"))
        };

        assert_eq!(
            jobs_gen::JOB_ITEM_R1,
            by_rarity("interactableItemSpawnGroups", 1)
        );
        assert_eq!(
            jobs_gen::JOB_ITEM_R2,
            by_rarity("interactableItemSpawnGroups", 2)
        );
        assert_eq!(
            jobs_gen::JOB_ITEM_R3,
            by_rarity("interactableItemSpawnGroups", 3)
        );
        assert_eq!(
            jobs_gen::JOB_SECRET_ITEM_R1,
            by_rarity("interactableItemSpawnGroupsInSecrets", 1)
        );
        assert_eq!(
            jobs_gen::JOB_SECRET_ITEM_R2,
            by_rarity("interactableItemSpawnGroupsInSecrets", 2)
        );
        assert_eq!(
            jobs_gen::JOB_SECRET_ITEM_R3,
            by_rarity("interactableItemSpawnGroupsInSecrets", 3)
        );

        // The enemy groups come from the same file and are pinned the same way.
        let enemy = |k: &str| -> Uuid {
            Uuid::parse_str(g["enemySpawnGroups"][k].as_str().unwrap()).unwrap()
        };
        assert_eq!(jobs_gen::JOB_PRIMARY_SPAWN_GROUP, enemy("primary"));
        assert_eq!(jobs_gen::JOB_SECONDARY_SPAWN_GROUP, enemy("secondary"));
        assert_eq!(jobs_gen::JOB_BOSS_SPAWN_GROUP, enemy("boss"));
        assert_eq!(jobs_gen::JOB_SECRET_BOSS_SPAWN_GROUP, enemy("secretBoss"));
    }

    /// AN ORDINARY JOB IS SENT 3-5 FLOOR-ITEM GROUPS, NOT ALL SEVEN.
    ///
    /// Rule 4, measured over the same 1395 captured non-duel jobs and authored in
    /// `job_pools.json` rather than fitted:
    ///
    /// ```text
    ///   both rarity-1 groups present            1395/1395
    ///   EXACTLY one of the secrets r2/r3        1395/1395
    ///   AT MOST one of the main r2/r3           1395/1395
    /// ```
    ///
    /// The first pass of this fix pruned the ENEMY groups only, so every ordinary
    /// job still over-sent 2-3 item groups — 1395 of 1395 of them.
    #[test]
    fn an_ordinary_job_gets_a_rarity_pick_of_floor_item_groups() {
        let gd = game_data();
        let jobs = board();
        let mut seen_sizes = std::collections::HashSet::new();
        let mut checked = 0usize;

        for job in jobs.iter().filter(|j| j["jobSetup"]["jobType"] != 5) {
            let data = jobs_gen::generated_data_for_job(&gd, job, &crate::quest::shipped_scaling())
                .expect("job generates");
            let has = |u: Uuid| data.item_generated_data.contains_key(&u);

            assert!(
                has(jobs_gen::JOB_ITEM_R1),
                "the main rarity-1 group is always present"
            );
            assert!(
                has(jobs_gen::JOB_SECRET_ITEM_R1),
                "the secret rarity-1 group is always present"
            );

            let secrets = [jobs_gen::JOB_SECRET_ITEM_R2, jobs_gen::JOB_SECRET_ITEM_R3]
                .into_iter()
                .filter(|u| has(*u))
                .count();
            assert_eq!(
                secrets, 1,
                "exactly one secret-room rarity group, never both or neither"
            );

            let main = [jobs_gen::JOB_ITEM_R2, jobs_gen::JOB_ITEM_R3]
                .into_iter()
                .filter(|u| has(*u))
                .count();
            assert!(main <= 1, "at most one main rarity group, never both");

            seen_sizes.insert(data.item_generated_data.len());
            checked += 1;
        }

        assert!(checked > 0, "the fixture board must contain ordinary jobs");
        // Retail ships 3-5 (3 when a Gather group is absent, up to 5 with it).
        for n in &seen_sizes {
            assert!(
                (3..=5).contains(n),
                "an ordinary job shipped {n} item groups, retail ships 3-5"
            );
        }
    }

    /// THE CONTROL: the pick is DETERMINISTIC per job. A roll reseeded on every
    /// request would change the scene under a client mid-dungeon, which is the
    /// class of fault this whole report is about.
    #[test]
    fn the_floor_item_pick_is_stable_across_regenerations() {
        let gd = game_data();
        let jobs = board();
        let job = jobs
            .iter()
            .find(|j| j["jobSetup"]["jobType"] != 5)
            .expect("an ordinary job");

        let first = jobs_gen::generated_data_for_job(&gd, job, &crate::quest::shipped_scaling())
            .expect("generates");
        for _ in 0..5 {
            let again =
                jobs_gen::generated_data_for_job(&gd, job, &crate::quest::shipped_scaling())
                    .expect("generates");
            let a: std::collections::BTreeSet<_> = first.item_generated_data.keys().collect();
            let b: std::collections::BTreeSet<_> = again.item_generated_data.keys().collect();
            assert_eq!(
                a, b,
                "the same job must always generate the same item groups"
            );
        }
    }

    /// AN ORDINARY JOB MUST NOT BE SENT THE SECRET-ROOM BOSS IT DOES NOT HAVE.
    ///
    /// Same defect the Duel fix removed, on the path that fix never touched. Retail,
    /// over 1395 captured non-duel jobs, is a perfect bijection with no
    /// counterexample: the secret-boss group is present exactly when
    /// `jobSetup.secretBossEnemyFamilyId` is, 233/233 yes and 0/1162 no.
    ///
    /// Keyed on that field and NOT on `secretRoom` — 373 captured jobs have a secret
    /// room and no secret boss, so the two conditions are not the same one.
    ///
    /// On the live board this was putting the secret-room boss into 219 of 564 job
    /// entries whose own setup says there is no secret room. Report #168.
    #[test]
    fn an_ordinary_job_only_gets_the_groups_its_setup_declares() {
        let gd = game_data();
        let jobs = board();
        let mut checked_without = 0usize;
        let mut checked_with = 0usize;

        for job in jobs.iter().filter(|j| j["jobSetup"]["jobType"] != 5) {
            let setup = &job["jobSetup"];
            let data = jobs_gen::generated_data_for_job(&gd, job, &crate::quest::shipped_scaling())
                .expect("job generates");
            let has_secret_boss = !setup["secretBossEnemyFamilyId"].is_null();
            let carries = data
                .enemy_generated_data
                .contains_key(&jobs_gen::JOB_SECRET_BOSS_SPAWN_GROUP);
            assert_eq!(
                carries, has_secret_boss,
                "secret-boss group must be present exactly when the job declares one"
            );
            if has_secret_boss {
                checked_with += 1;
            } else {
                checked_without += 1;
            }

            // The Gather pickup follows the same shape.
            let has_gather = !setup["gatherItemId"].is_null();
            assert_eq!(
                data.item_generated_data
                    .contains_key(&jobs_gen::JOB_GATHER_ITEM_SPAWN_GROUP),
                has_gather,
                "Gather item group must be present exactly when the job declares one"
            );

            // The packs carry the job's own declared counts, not the reference's 6/4.
            for (group, key) in [
                (jobs_gen::JOB_PRIMARY_SPAWN_GROUP, "primaryEnemyCount"),
                (jobs_gen::JOB_SECONDARY_SPAWN_GROUP, "secondaryEnemyCount"),
            ] {
                if let (Some(spawners), Some(want)) =
                    (data.enemy_generated_data.get(&group), setup[key].as_i64())
                {
                    assert_eq!(
                        spawners.len() as i64,
                        want,
                        "{key}: spawner count must match the job's declared count"
                    );
                }
            }

            // The boss is levelled, as the Duel's already was.
            let difficulty = job["difficultyLevel"].as_i64().unwrap_or(1);
            let boss_delta = setup["bossLevelDelta"].as_i64().unwrap_or(0);
            if let Some(spawners) = data
                .enemy_generated_data
                .get(&jobs_gen::JOB_BOSS_SPAWN_GROUP)
            {
                assert!(
                    spawners
                        .iter()
                        .flatten()
                        .all(|e| e.enemy_level == difficulty + boss_delta),
                    "the job boss must sit at difficultyLevel + bossLevelDelta"
                );
            }
        }

        // THE CONTROL. A board that happened to contain only one kind of job would
        // satisfy every assertion above while proving nothing about the other kind.
        // Both cases must actually occur, or this test is vacuous.
        assert!(
            checked_without > 0,
            "the fixture board must contain a job with NO secret boss"
        );
        assert!(
            checked_with > 0,
            "the fixture board must contain a job WITH a secret boss — otherwise the \
             bijection is only tested in one direction"
        );
    }

    /// The measured identity of the reference dungeon, pinned.
    ///
    /// In capture-599 all six job entries' spawn ids are subsets of
    /// `JobSpawnGroupsReference`, and the two story quests in the same body are subsets of
    /// neither — that discriminating pair is what identified it. Same assertion here
    /// against our own output, so swapping in a different dungeon fails.
    #[test]
    fn job_spawn_ids_come_from_the_reference_dungeon_and_story_quests_do_not() {
        let gd = game_data();
        let reference = gd
            .dungeons
            .get(&jobs_gen::JOB_SPAWN_GROUPS_REFERENCE)
            .expect("JobSpawnGroupsReference is in parsed.json");
        assert_eq!(
            reference.handle, "JobSpawnGroupsReference",
            "the id still names it"
        );
        let enemies: HashSet<Uuid> = reference
            .spawn_info
            .enemy_spawn_groups
            .keys()
            .copied()
            .collect();

        for job in board() {
            let data =
                jobs_gen::generated_data_for_job(&gd, &job, &crate::quest::shipped_scaling())
                    .expect("generated");
            for id in data.enemy_generated_data.keys() {
                assert!(
                    enemies.contains(id),
                    "job spawn id {id} is not in the reference dungeon"
                );
            }
        }

        // Control: a story quest's ids are NOT the reference dungeon's. Without this a
        // reference containing *every* spawn id in the game would pass the loop above.
        let story = story_generated(&gd);
        assert!(
            story
                .enemy_generated_data
                .keys()
                .all(|id| !enemies.contains(id)),
            "the control quest must draw from its OWN dungeon, not the job reference"
        );
    }

    /// The stored row carries the data too, so /accept and the dungeon-enter path can
    /// resolve a job. It used to persist `None` unconditionally.
    #[test]
    fn a_stored_job_row_carries_its_generated_data() {
        let gd = game_data();
        for job in board() {
            let entry =
                jobs_gen::job_quest_db_entry(&job, CHAR, &gd, &crate::quest::shipped_scaling())
                    .expect("row builds");
            let data = entry
                .generated_data
                .0
                .unwrap_or_else(|| panic!("job row {} persisted no generated data", entry.id));
            assert!(!data.enemy_generated_data.is_empty());
        }
    }

    /// Retail stamps `version: 1` on every generated-data entry in the captured body.
    /// Our story quests have always shipped 0 and work, so this is cosmetic and scoped to
    /// the job path — the control pins that the quest path is untouched.
    #[test]
    fn job_entries_carry_the_captured_version_and_story_quests_are_unchanged() {
        let gd = game_data();
        for job in board() {
            let data =
                jobs_gen::generated_data_for_job(&gd, &job, &crate::quest::shipped_scaling())
                    .expect("generated");
            assert_eq!(data.version, 1, "retail sends version 1 on job entries");
            assert_eq!(data.algorithm_version, 1);
        }
        assert_eq!(
            story_generated(&gd).version,
            0,
            "the story-quest path must not shift"
        );
    }

    /// The key set retail puts on a job's generated-data entry, pinned.
    ///
    /// From capture-599: every one of the six job entries carried exactly
    /// `algorithmVersion, chestGeneratedData, enemyGeneratedData, itemGeneratedData,
    /// questId, version` — zero missing, zero extra against ours. Note what is NOT there:
    /// no `gldQuestId`. The two story quests in the same body DO carry one on their
    /// `quests[]` entry, which is how we know the missing link was the generated data and
    /// not a missing gldQuestId on the jobs.
    #[test]
    fn a_job_entry_serializes_to_retails_key_set() {
        let gd = game_data();
        let jobs = board();
        let list =
            assemble_generated_data_list(Vec::new(), &gd, &jobs, &crate::quest::shipped_scaling());
        let entry = serde_json::to_value(&list[0]).expect("entry serializes");
        let mut keys: Vec<&str> = entry
            .as_object()
            .expect("object")
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "algorithmVersion",
                "chestGeneratedData",
                "enemyGeneratedData",
                "itemGeneratedData",
                "questId",
                "version",
            ],
            "job generated-data key set must match capture-599's six job entries",
        );
    }

    /// The invariant as the client sees it, on the serialized response: every id in
    /// `jobs[]` appears in `dungeonGeneratedDataList[]`, and so does every id in
    /// `quests[]`.
    #[test]
    fn the_serialized_response_resolves_every_job_and_every_quest() {
        let gd = game_data();
        let jobs = board();

        // A story quest advertised alongside the jobs, exactly as a real board is.
        let story_id = Uuid::from_u128(0x570F);
        let (story_quest, story_data) = generate_quest_data(
            &gd,
            story_quest_with_dungeon(&gd),
            48,
            &QuestLevelScaling::default(),
        )
        .expect("control quest generates");
        // Assembled exactly the way the handler assembles it: the story quest arrives via
        // `split_quest_rows` (stored row), the jobs are added by the same call the route
        // makes. Going through `assemble_generated_data_list` rather than reaching past it
        // is the point — the route needs a DB, so this function is the closest testable
        // seam to the wire.
        let (quests_out, _events, from_rows) = split_quest_rows(
            vec![(story_id, story_quest.clone(), story_data.clone())].into_iter(),
            &Default::default(),
        );
        let generated =
            assemble_generated_data_list(from_rows, &gd, &jobs, &crate::quest::shipped_scaling());

        assert_eq!(quests_out.len(), 1, "the control quest is advertised");

        let body = serde_json::to_value(GetQuestsResponse {
            quests: quests_out,
            dungeon_generated_data_list: generated,
            jobs: jobs.clone(),
            character: blades_lib::user_data::CompleteCharacterWithIdWithoutData {
                id: Uuid::nil(),
                character: Default::default(),
            },
            job_pools: json!([]),
            deleted_quest_ids: vec![],
            game_event_quests: vec![],
            game_event_quests_in_warning: vec![],
            game_event_quests_finished: vec![],
        })
        .expect("response serializes");

        let resolvable: HashSet<&str> = body["dungeonGeneratedDataList"]
            .as_array()
            .expect("list present")
            .iter()
            .map(|e| e["questId"].as_str().expect("entry has a questId"))
            .collect();

        for job in body["jobs"].as_array().expect("jobs") {
            let id = job["questId"].as_str().unwrap();
            assert!(
                resolvable.contains(id),
                "job {id} is on the board but unresolvable"
            );
        }
        // The control, in the same assertion style: quests must resolve too. A change
        // that emptied the whole list would pass the loop above only if `jobs` were also
        // empty, and this catches the case where it is not.
        for quest in body["quests"].as_array().expect("quests") {
            let id = quest["questId"].as_str().unwrap();
            assert!(
                resolvable.contains(id),
                "quest {id} is advertised but unresolvable"
            );
        }
        assert_eq!(
            body["jobs"].as_array().unwrap().len() + body["quests"].as_array().unwrap().len(),
            resolvable.len(),
            "one entry per advertised thing, as in capture-599 (6 jobs + 2 quests + 2 events = 10)"
        );
    }
}

#[cfg(test)]
mod event_quest_tests {
    use super::*;
    use blades_lib::static_data::StaticData;

    /// The committed static data, loaded the way the server loads it.
    fn static_data() -> StaticData {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        crate::static_loader::load(&dir)
    }

    fn game_data() -> blades_lib::game_data::GameData {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid parsed.json")
    }

    const CHAR: Uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    /// 2026-05-03 00:00 UTC — inside the corpus's own event calendar.
    const NOW: i64 = 1_777_852_800;

    /// A quest body built through serde from the exact key set production stores.
    fn quest(gld: Uuid) -> blades_lib::user_data::Quest {
        serde_json::from_value(json!({
            "version": 0,
            "type": "NORMAL",
            "objectiveStatuses": {},
            "difficultyLevel": 0,
            "seed": 0,
            "gldQuestId": gld,
            "completed": false,
        }))
        .expect("fixture quest must deserialize")
    }

    fn generated() -> blades_lib::user_data::DungeonGeneratedData {
        serde_json::from_value(json!({ "algorithmVersion": 0, "version": 0 }))
            .expect("minimal generated data must deserialize")
    }

    /// THE gldQuestId gotcha, pinned.
    ///
    /// An event quest's `questId` is a per-character instance that resolves to
    /// NOTHING in `parsed.json`; its `gldQuestId` is the template that carries the
    /// objectives, the dungeon, the version and the rewards. Retail's corpus is
    /// unambiguous: 1271 of the event-quest entries had `questId != gldQuestId` and
    /// not one had them equal.
    #[test]
    fn an_event_quest_instance_id_is_not_its_template_id() {
        let sd = static_data();
        let gd = game_data();
        let minted = event_quests::mint(&sd, &gd, CHAR, 40, NOW);
        assert!(
            !minted.is_empty(),
            "the committed calendar opens events at NOW"
        );
        for m in &minted {
            assert_ne!(
                m.quest_id, m.quest.gld_quest_id,
                "the instance id must differ from the template id"
            );
            assert!(
                gd.quests.contains_key(&m.quest.gld_quest_id),
                "the TEMPLATE resolves in parsed.json"
            );
            assert!(
                !gd.quests.contains_key(&m.quest_id),
                "the INSTANCE must not — that is the whole point of the two ids"
            );
            assert!(matches!(
                m.quest.r#type,
                blades_lib::user_data::QuestType::GameEvent
            ));
            let data = m
                .quest
                .game_event_quest_data
                .as_ref()
                .expect("carries its instance");
            assert!(data.game_event_instance_id.contains("::"));
        }
    }

    /// Under a themed window the quest rows follow the window exactly as the
    /// `/gameevents` feed does: the same instances open and announced, every one
    /// of them playable, and every Halloween event minted on two or more separate
    /// windows (each a fresh row, so a fresh five-completion ladder).
    #[test]
    fn a_themed_window_mints_the_same_instances_the_feed_serves() {
        use blades_lib::features::game_events::{
            EventTheme, HALLOWEEN_THEME_QUESTS, WARNING_LEAD_SECS, active_events_themed,
            upcoming_events_themed,
        };
        use std::collections::{BTreeMap, BTreeSet};

        let mut sd = static_data();
        let gd = game_data();
        sd.game_event_theme = Some(EventTheme {
            start_secs: 1_791_590_400, // 2026-10-10 00:00 UTC
            end_secs: 1_793_404_800,   // 2026-10-31 00:00 UTC
            quest_ids: HALLOWEEN_THEME_QUESTS.to_vec(),
        });
        let theme = sd.game_event_theme.clone().unwrap();
        let instance = |q: &blades_lib::user_data::Quest| {
            q.game_event_quest_data
                .as_ref()
                .unwrap()
                .game_event_instance_id
                .clone()
        };

        let mut rows: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
        let mut now = theme.start_secs;
        while now < theme.end_secs {
            let feed: Vec<String> = active_events_themed(&sd.game_events, Some(&theme), now)
                .into_iter()
                .map(|e| e.game_event_instance_id)
                .collect();
            let minted = event_quests::mint(&sd, &gd, CHAR, 40, now);
            let got: Vec<String> = minted.iter().map(|m| instance(&m.quest)).collect();
            assert_eq!(got, feed, "t={now}: quest rows and feed disagree");
            for m in &minted {
                assert!(
                    !m.quest.objective_statuses.is_empty(),
                    "t={now}: no objectives"
                );
                assert!(m.dungeon.is_some(), "t={now}: no dungeon");
                rows.entry(m.quest.gld_quest_id)
                    .or_default()
                    .insert(instance(&m.quest));
            }

            let soon: Vec<String> =
                upcoming_events_themed(&sd.game_events, Some(&theme), now, WARNING_LEAD_SECS)
                    .into_iter()
                    .map(|e| e.game_event_instance_id)
                    .collect();
            let warned: Vec<String> = event_quests::upcoming(&sd, &gd, CHAR, 40, now)
                .iter()
                .map(|q| instance(&q.quest))
                .collect();
            assert_eq!(warned, soon, "t={now}: warning row and feed disagree");
            now += 6 * 3_600;
        }
        for q in HALLOWEEN_THEME_QUESTS {
            let n = rows.get(&q).map_or(0, |s| s.len());
            assert!(n >= 2, "{q} minted on {n} window(s)");
        }
    }

    /// Every open event must be *playable*: objectives and dungeon data, resolved
    /// through the template. Advertising an event with no dungeon data is what hangs
    /// the client's quest map (report #62), and the instance id resolves to nothing,
    /// so a lookup on the wrong id produces exactly that.
    #[test]
    fn every_minted_event_quest_has_objectives_and_dungeon_data() {
        let sd = static_data();
        let gd = game_data();
        for m in event_quests::mint(&sd, &gd, CHAR, 40, NOW) {
            assert!(
                !m.quest.objective_statuses.is_empty(),
                "event quest {} has no objectives",
                m.quest_id
            );
            assert!(
                m.dungeon.is_some(),
                "event quest {} has no dungeon data — the client would wait forever",
                m.quest_id
            );
            assert!(
                m.quest.rewards.is_some(),
                "milestones must reach the client"
            );
            assert_eq!(
                m.quest.rewards.as_ref().unwrap().len(),
                5,
                "five milestones"
            );
            assert!(m.quest.final_reward.is_some());
        }
    }

    /// The instance id is stable for a character within a window and different across
    /// characters and across windows. Without stability every `/quests` poll would
    /// mint a new row and reset the player's progress.
    #[test]
    fn instance_ids_are_stable_per_character_and_window() {
        let a = event_quests::instance_quest_id(CHAR, "e1::1000");
        assert_eq!(
            a,
            event_quests::instance_quest_id(CHAR, "e1::1000"),
            "stable"
        );
        assert_ne!(
            a,
            event_quests::instance_quest_id(CHAR, "e1::2000"),
            "next window differs"
        );
        let other = Uuid::from_u128(0xdead_beef);
        assert_ne!(
            a,
            event_quests::instance_quest_id(other, "e1::1000"),
            "per character"
        );
    }

    /// A GAME_EVENT row goes to `gameEventQuests[]`, never `quests[]` — and only
    /// while its window is open.
    #[test]
    fn event_rows_are_routed_to_the_event_array_and_expire_with_their_window() {
        let open = Uuid::from_u128(0x0E1);
        let closed = Uuid::from_u128(0x0E2);
        let normal = Uuid::from_u128(0x0A1);
        let mut event = quest(Uuid::from_u128(0xC0FFEE));
        event.r#type = blades_lib::user_data::QuestType::GameEvent;

        let mut live = std::collections::HashSet::new();
        live.insert(open);

        let (quests, events, generated_data) = split_quest_rows(
            vec![
                (open, event.clone(), Some(generated())),
                (closed, event, Some(generated())),
                (normal, quest(Uuid::from_u128(0xA1)), Some(generated())),
            ]
            .into_iter(),
            &live,
        );
        assert_eq!(quests.len(), 1, "only the ordinary quest is in quests[]");
        assert_eq!(quests[0].quest_id, normal);
        assert_eq!(events.len(), 1, "the open event, and only it");
        assert_eq!(events[0].quest_id, open);
        // The client's invariant still holds across BOTH arrays.
        let mut advertised: Vec<Uuid> = quests
            .iter()
            .chain(events.iter())
            .map(|q| q.quest_id)
            .collect();
        let mut have: Vec<Uuid> = generated_data.iter().map(|g| g.quest_id).collect();
        advertised.sort();
        have.sort();
        assert_eq!(advertised, have);
    }

    /// The milestone ladder: the Nth completion pays the Nth tier, the last one adds
    /// the final reward, and past the end the instance pays nothing.
    #[test]
    fn an_event_quest_pays_its_milestones_in_order() {
        let sd = static_data();
        let gd = game_data();
        let m = event_quests::mint(&sd, &gd, CHAR, 40, NOW)
            .into_iter()
            .next()
            .expect("an open event");
        let tmpl = sd
            .event_quests
            .templates
            .get(&m.quest.gld_quest_id)
            .expect("template shipped");

        let mut paid = Vec::new();
        for n in 0..6 {
            paid.push(event_milestone_reward(&sd, m.quest_id, &m.quest, n).unwrap_or_default());
        }
        for tier in 0..5 {
            assert!(
                !paid[tier].is_empty(),
                "milestone {tier} must pay something"
            );
        }
        assert!(
            paid[5].is_empty(),
            "a sixth completion pays nothing — the instance is exhausted"
        );
        // Successive milestones are distinct, i.e. it is a ladder and not the same
        // reward five times (which is what a fixed quest_rewards lookup would give).
        assert_ne!(
            serde_json::to_value(&paid[0]).unwrap(),
            serde_json::to_value(&paid[1]).unwrap(),
            "milestone 1 and 2 must differ"
        );
        // The last one carries the finalReward on top of the last tier.
        let final_reward = tmpl.final_reward.as_ref().expect("shipped");
        let last = serde_json::to_value(&paid[4]).unwrap();
        for (id, n) in &final_reward.stackable_items {
            let got = last["stackableItems"][id.to_string()].as_u64().unwrap_or(0)
                + last["currencies"][id.to_string()].as_u64().unwrap_or(0);
            assert!(
                got >= *n,
                "the last milestone must include the finalReward's {n} of {id}, got {got}"
            );
        }
    }

    /// Report #324: the last tier paid `finalReward` twice and granted gold or gems
    /// as a backpack item, and the client's rewards screen never closed. These are
    /// the 13 captured retail final-tier `/complete` rewards whose template ships
    /// the same `finalReward` (snapshot 20260607, `api_captures` ids); ours must be
    /// the same grant, field for field.
    #[test]
    fn the_last_tier_pays_what_retail_paid_on_its_final_complete() {
        let sd = static_data();
        let cases: &[(u32, &str, &str)] = &[
            (1928, "a846b491-b439-447d-a8a1-9c2611522610", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":27,"f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2":63000},"stackableItems":{"bafe6ed5-6473-4a4c-aef5-421d3af5c8cb":5}}"#),
            (14396, "0dd37f6b-28ad-4cb7-8e12-2ebbf43fb366", r#"{"characterXp":700,"currencies":{"470c8f58-a8dd-4c07-8c92-843b785e1139":36,"c64bcb53-41f4-41ba-892a-fe2cca423caa":15},"stackableItems":{"b74a5c55-a687-4604-aa59-ba3ddfddcd2a":96}}"#),
            (27198, "26eb6ab5-2d8c-4993-820e-ada79f6f00a8", r#"{"characterXp":700,"currencies":{"470c8f58-a8dd-4c07-8c92-843b785e1139":36,"c64bcb53-41f4-41ba-892a-fe2cca423caa":20},"stackableItems":{"19ce1a65-057f-4f34-a0ed-27de7c085662":5}}"#),
            (34025, "2d1200ee-ecb8-4ea6-9892-54d1f538d83d", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":16},"stackableItems":{"f11fb90b-b441-4d72-a33f-50d14d3d6778":140,"fd67bbc6-20f4-44a3-9614-28265ebb8c67":216}}"#),
            (36532, "816ff4c8-b56f-4645-bd2a-29bc7c1baf96", r#"{"characterXp":700,"currencies":{"470c8f58-a8dd-4c07-8c92-843b785e1139":36,"c64bcb53-41f4-41ba-892a-fe2cca423caa":16,"f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2":25200}}"#),
            (38669, "a85408a6-7107-433d-b616-50105080574e", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":16},"stackableItems":{"3ec6cf6f-d90e-4b76-bb7f-82da251ab5e5":3,"fd67bbc6-20f4-44a3-9614-28265ebb8c67":216}}"#),
            (39364, "f4023a38-195a-4977-bfbc-f443a257566f", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":20,"f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2":38000},"stackableItems":{"21e6557f-17ca-4bd3-9379-00184efe0edc":24}}"#),
            (40877, "a008cbbc-a164-4183-8b69-470ac8cd5707", r#"{"characterXp":700,"currencies":{"470c8f58-a8dd-4c07-8c92-843b785e1139":15,"c64bcb53-41f4-41ba-892a-fe2cca423caa":21},"stackableItems":{"e7193116-d761-479b-8a20-5633737977f5":410}}"#),
            (43620, "2290ab73-8f56-4dba-a4f0-7a23270eb93c", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":22},"stackableItems":{"e7193116-d761-479b-8a20-5633737977f5":372,"fd67bbc6-20f4-44a3-9614-28265ebb8c67":560}}"#),
            (48242, "9181f784-9eb8-4872-953d-30ba8b6bc9d1", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":32},"stackableItems":{"16e102fb-b1c0-42de-8106-0aa27e77f7f0":3,"85ed5500-3581-4699-8095-4b5ff6514355":96}}"#),
            (49507, "a1aafdc9-a35c-45c9-89f6-27f7d3a628e6", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":16},"stackableItems":{"1c5c5ce3-178b-4938-89f3-faf3fa7f0664":24,"e7193116-d761-479b-8a20-5633737977f5":410}}"#),
            (52104, "cd66c93c-5086-4311-b0f6-e90af54099b2", r#"{"characterXp":700,"currencies":{"470c8f58-a8dd-4c07-8c92-843b785e1139":36,"c64bcb53-41f4-41ba-892a-fe2cca423caa":24},"stackableItems":{"790a188b-3fa0-4f38-99d9-bc8d3675bc46":5}}"#),
            (62711, "01121c2f-6806-4b8c-998e-adc5d0e21db7", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":16},"stackableItems":{"16e102fb-b1c0-42de-8106-0aa27e77f7f0":3,"e7193116-d761-479b-8a20-5633737977f5":276}}"#),
        ];
        for (capture, gld, retail) in cases {
            let gld: Uuid = gld.parse().unwrap();
            let tiers = sd.event_quests.templates[&gld].milestone_count();
            let paid = event_milestone_reward(&sd, Uuid::from_u128(0xE7), &quest(gld), tiers - 1)
                .expect("the last tier pays");
            let retail: RewardGrant = serde_json::from_str(retail).unwrap();
            assert_eq!(paid, retail, "capture {capture} ({gld}): final tier");
        }
    }

    const SIGIL: &str = "c64bcb53-41f4-41ba-892a-fe2cca423caa";

    /// Retail's sigil ladder per level band, keyed by the event's first-tier sigil
    /// count (its "family"). Measured 2026-10-04 over every captured event instance
    /// (snapshot 20260607 + prod `api_captures`): no captured ladder falls outside
    /// this table, and each row matches the character's level — band 1 at levels
    /// 10-15, band 2 at 17-25, band 3 at 26-35, band 4 at 37-43, band 5 at 48-100.
    /// Family 5's band 1 was never captured.
    const RETAIL_SIGIL_BANDS: &[(u64, [Option<[u64; 5]>; 5])] = &[
        (1, [Some([1, 2, 5, 7, 10]), Some([1, 3, 6, 9, 12]), Some([1, 4, 7, 11, 16]), Some([1, 4, 8, 12, 18]), Some([1, 4, 9, 14, 22])]),
        (3, [Some([3, 4, 7, 10, 15]), Some([3, 6, 9, 12, 16]), Some([3, 6, 9, 15, 21]), Some([3, 6, 10, 16, 24]), Some([3, 6, 12, 18, 27])]),
        (4, [Some([4, 5, 8, 12, 17]), Some([4, 6, 10, 14, 20]), Some([4, 7, 11, 17, 24]), Some([4, 7, 12, 19, 29]), Some([4, 8, 14, 22, 34])]),
        (5, [None, Some([5, 8, 12, 18, 26]), Some([5, 8, 14, 21, 32]), Some([5, 9, 15, 24, 37]), Some([5, 9, 16, 27, 43])]),
    ];

    fn sigils_paid(r: &RewardGrant) -> u64 {
        let id: Uuid = SIGIL.parse().unwrap();
        r.currencies.get(&id).copied().unwrap_or(0) + r.stackable_items.get(&id).copied().unwrap_or(0)
    }

    /// Every event row minted for a character of `level` over 50 days of the
    /// calendar, with the sigils each of its five completions pays.
    fn sigils_by_event_at(level: i64) -> std::collections::BTreeMap<Uuid, Vec<u64>> {
        let (sd, gd) = (static_data(), game_data());
        let mut out = std::collections::BTreeMap::new();
        for day in 0..50 {
            for m in event_quests::mint(&sd, &gd, CHAR, level, NOW + day * 86_400) {
                out.entry(m.quest.gld_quest_id).or_insert_with(|| {
                    (0..5)
                        .map(|n| {
                            event_milestone_reward(&sd, m.quest_id, &m.quest, n)
                                .map_or(0, |r| sigils_paid(&r))
                        })
                        .collect()
                });
            }
        }
        out
    }

    /// Report #333: "Events are not awarding max sigils to my level 57 character."
    /// On prod his level-57 character was paid 4/6/10/14/20 for The Seventh Seal
    /// (7486296a) — band 2, a level 16-25 ladder. Retail paid level 46+ the top
    /// band of every event; a level-10 character the bottom one.
    #[test]
    fn report_333_events_pay_the_sigil_band_of_the_characters_level() {
        let family = |paid: &[u64]| RETAIL_SIGIL_BANDS.iter().find(|(f, _)| *f == paid[0]);
        for (level, band) in [(57i64, 4usize), (10, 0)] {
            let paid = sigils_by_event_at(level);
            assert!(paid.len() >= 30, "level {level}: only {} events minted", paid.len());
            let mut wrong = Vec::new();
            let mut checked = 0;
            for (gld, ladder) in &paid {
                let (_, bands) = family(ladder).unwrap_or_else(|| panic!("{gld}: family {ladder:?}"));
                let Some(want) = bands[band] else { continue };
                checked += 1;
                if ladder[..] != want[..] {
                    wrong.push(format!("{gld}: paid {ladder:?}, retail {want:?}"));
                }
            }
            assert!(checked >= 30, "level {level}: checked {checked}");
            assert!(wrong.is_empty(), "level {level}, {} of {checked} events:\n{}", wrong.len(), wrong.join("\n"));
        }
    }

    /// Identity test for the shipped data: every band of every template pays its
    /// family's retail sigil ladder, in order, with the measured thresholds.
    #[test]
    fn report_333_every_shipped_band_is_a_retail_sigil_band() {
        let sd = static_data();
        for (gld, tmpl) in &sd.event_quests.templates {
            let mins: Vec<i64> = tmpl.level_bands.iter().map(|b| b.min_level).collect();
            assert_eq!(mins, [1, 16, 26, 36, 46], "{gld}");
            let fam = sigils_paid(&tmpl.level_bands[4].rewards[0]);
            let (_, rows) = RETAIL_SIGIL_BANDS.iter().find(|(f, _)| *f == fam).expect("family");
            for (i, band) in tmpl.level_bands.iter().enumerate() {
                let got: Vec<u64> = band.rewards.iter().map(sigils_paid).collect();
                match rows[i] {
                    Some(want) => assert_eq!(got, want, "{gld} band {}", i + 1),
                    // Family 5's band 1 is not known; it stays at band 2's.
                    None => assert_eq!(Some(got.as_slice()), rows[i + 1].as_ref().map(|r| &r[..]), "{gld} band 1"),
                }
            }
        }
    }

    /// The band boundaries: 16, 26, 36, 46. 25 -> 26 is pinned by one character's
    /// consecutive mints; the others sit inside the measured gaps.
    #[test]
    fn report_333_band_thresholds() {
        let sd = static_data();
        let tmpl = &sd.event_quests.templates[&"7486296a-335a-4b46-aee1-5102090ce34f".parse::<Uuid>().unwrap()];
        let first_tier = |level| sigils_paid(&tmpl.rewards_for_level(level)[4]);
        for (level, want) in [(0, 17), (1, 17), (15, 17), (16, 20), (25, 20), (26, 24), (35, 24), (36, 29), (45, 29), (46, 34), (57, 34), (100, 34)] {
            assert_eq!(first_tier(level), want, "level {level}");
        }
    }

    /// Retail fixed the ladder when it minted the instance: two of these were minted
    /// at level 24 and finished at 26, and paid band 2. These are the 13 captured
    /// final-tier `/complete` rewards of #324 again, each against a row minted at the
    /// level its character had when the instance first appears in the corpus.
    #[test]
    fn report_333_an_instance_pays_the_band_it_was_minted_at() {
        let sd = static_data();
        let cases: &[(u32, i64, &str, &str)] = &[
            (1928, 86, "a846b491-b439-447d-a8a1-9c2611522610", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":27,"f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2":63000},"stackableItems":{"bafe6ed5-6473-4a4c-aef5-421d3af5c8cb":5}}"#),
            (14396, 10, "0dd37f6b-28ad-4cb7-8e12-2ebbf43fb366", r#"{"characterXp":700,"currencies":{"470c8f58-a8dd-4c07-8c92-843b785e1139":36,"c64bcb53-41f4-41ba-892a-fe2cca423caa":15},"stackableItems":{"b74a5c55-a687-4604-aa59-ba3ddfddcd2a":96}}"#),
            (27198, 19, "26eb6ab5-2d8c-4993-820e-ada79f6f00a8", r#"{"characterXp":700,"currencies":{"470c8f58-a8dd-4c07-8c92-843b785e1139":36,"c64bcb53-41f4-41ba-892a-fe2cca423caa":20},"stackableItems":{"19ce1a65-057f-4f34-a0ed-27de7c085662":5}}"#),
            (34025, 22, "2d1200ee-ecb8-4ea6-9892-54d1f538d83d", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":16},"stackableItems":{"f11fb90b-b441-4d72-a33f-50d14d3d6778":140,"fd67bbc6-20f4-44a3-9614-28265ebb8c67":216}}"#),
            (36532, 23, "816ff4c8-b56f-4645-bd2a-29bc7c1baf96", r#"{"characterXp":700,"currencies":{"470c8f58-a8dd-4c07-8c92-843b785e1139":36,"c64bcb53-41f4-41ba-892a-fe2cca423caa":16,"f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2":25200}}"#),
            (38669, 24, "a85408a6-7107-433d-b616-50105080574e", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":16},"stackableItems":{"3ec6cf6f-d90e-4b76-bb7f-82da251ab5e5":3,"fd67bbc6-20f4-44a3-9614-28265ebb8c67":216}}"#),
            (39364, 24, "f4023a38-195a-4977-bfbc-f443a257566f", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":20,"f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2":38000},"stackableItems":{"21e6557f-17ca-4bd3-9379-00184efe0edc":24}}"#),
            (40877, 26, "a008cbbc-a164-4183-8b69-470ac8cd5707", r#"{"characterXp":700,"currencies":{"470c8f58-a8dd-4c07-8c92-843b785e1139":15,"c64bcb53-41f4-41ba-892a-fe2cca423caa":21},"stackableItems":{"e7193116-d761-479b-8a20-5633737977f5":410}}"#),
            (43620, 86, "2290ab73-8f56-4dba-a4f0-7a23270eb93c", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":22},"stackableItems":{"e7193116-d761-479b-8a20-5633737977f5":372,"fd67bbc6-20f4-44a3-9614-28265ebb8c67":560}}"#),
            (48242, 28, "9181f784-9eb8-4872-953d-30ba8b6bc9d1", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":32},"stackableItems":{"16e102fb-b1c0-42de-8106-0aa27e77f7f0":3,"85ed5500-3581-4699-8095-4b5ff6514355":96}}"#),
            (49507, 28, "a1aafdc9-a35c-45c9-89f6-27f7d3a628e6", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":16},"stackableItems":{"1c5c5ce3-178b-4938-89f3-faf3fa7f0664":24,"e7193116-d761-479b-8a20-5633737977f5":410}}"#),
            (52104, 29, "cd66c93c-5086-4311-b0f6-e90af54099b2", r#"{"characterXp":700,"currencies":{"470c8f58-a8dd-4c07-8c92-843b785e1139":36,"c64bcb53-41f4-41ba-892a-fe2cca423caa":24},"stackableItems":{"790a188b-3fa0-4f38-99d9-bc8d3675bc46":5}}"#),
            (62711, 30, "01121c2f-6806-4b8c-998e-adc5d0e21db7", r#"{"characterXp":700,"currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":16},"stackableItems":{"16e102fb-b1c0-42de-8106-0aa27e77f7f0":3,"e7193116-d761-479b-8a20-5633737977f5":276}}"#),
        ];
        let mut wrong = Vec::new();
        for (capture, minted_at, gld, retail) in cases {
            let gld: Uuid = gld.parse().unwrap();
            let tmpl = &sd.event_quests.templates[&gld];
            let mut row = quest(gld);
            row.rewards = Some(tmpl.rewards_for_level(*minted_at));
            row.final_reward = tmpl.final_reward_for_level(*minted_at);
            // Finished two levels later: the row's band still decides.
            let paid = event_milestone_reward(&sd, Uuid::from_u128(0xE7), &row, 4).expect("last tier pays");
            let retail: RewardGrant = serde_json::from_str(retail).unwrap();
            // Exact, materials included. 9181f784's band 3 was once exempted here as
            // a "seasonally rotated material" (Moonstone in one capture, Malachite in
            // the other); it was the level — 27 and 28, either side of the Glass
            // unlock — and the per-level material now pays it exactly (#362).
            let same = paid == retail;
            if !same {
                wrong.push(format!("capture {capture} ({gld}, minted at {minted_at}): paid {}, retail {}",
                    serde_json::to_string(&paid).unwrap(), serde_json::to_string(&retail).unwrap()));
            }
        }
        assert!(wrong.is_empty(), "{} of {}:\n{}", wrong.len(), cases.len(), wrong.join("\n"));
    }

    /// The milestone + final material a row minted at `level` actually PAYS on its
    /// last completion — through `event_milestone_reward`, the grant path, not just
    /// the ladder the row displays.
    fn materials_paid_at(sd: &StaticData, gld: Uuid, level: i64) -> std::collections::HashMap<Uuid, u64> {
        let tmpl = &sd.event_quests.templates[&gld];
        let mut row = quest(gld);
        row.rewards = Some(tmpl.rewards_for_level(level));
        row.final_reward = tmpl.final_reward_for_level(level);
        let last = tmpl.milestone_count() - 1;
        event_milestone_reward(sd, Uuid::from_u128(0x362), &row, last)
            .expect("the last tier pays")
            .stackable_items
    }

    const ORICHALCUM: &str = "74f091b5-fd88-464b-a98a-f60a5e8a0f25";
    const DWARVEN: &str = "f11fb90b-b441-4d72-a33f-50d14d3d6778";
    const QUICKSILVER: &str = "e80bee76-f92c-4005-9eff-20d1e8c64d24";
    const EBONY: &str = "75112030-b248-49b0-9c70-0da8dea150d1";
    const DRAGON_BONES: &str = "d523932f-8c7f-4192-9112-5dbd60883c2b";

    /// Report #362: "today's event gives me Ebony ingots; that does not match my
    /// level" — Sephoris, level 26, The Spectral Forest (7f0d1508) on 2026-10-07.
    ///
    /// Retail paid this event's metal by the claimant's own level, stepping at the
    /// gear unlock levels: Orichalcum at 15, Ebony at 33-34, Dragon Bones at 50+
    /// (18 retail characters). We paid the 26-35 band's one captured material, Ebony,
    /// from level 26 up. Tier 5 is 96 ingots at every level; only the metal moves.
    #[test]
    fn report_362_the_spectral_forest_pays_the_claimants_own_metal() {
        let sd = static_data();
        let gld: Uuid = "7f0d1508-312b-4036-970f-ff5f4c342526".parse().unwrap();
        for (level, metal) in [
            (15, ORICHALCUM),
            (20, DWARVEN),
            (26, QUICKSILVER),
            (35, EBONY),
            (60, DRAGON_BONES),
            (100, DRAGON_BONES),
        ] {
            let paid = materials_paid_at(&sd, gld, level);
            let metal: Uuid = metal.parse().unwrap();
            assert_eq!(paid.get(&metal), Some(&96), "level {level}: paid {paid:?}");
            assert_eq!(paid.len(), 1, "level {level}: one metal, nothing else: {paid:?}");
        }
        // The report itself: no Ebony below Ebony's unlock level.
        let ebony: Uuid = EBONY.parse().unwrap();
        for level in [20, 26, 32] {
            assert!(!materials_paid_at(&sd, gld, level).contains_key(&ebony), "level {level} paid Ebony");
        }
    }

    /// Report #367: soul gems "should be Greater, I only get Common" — Sephoris on
    /// 26eb6ab5 (2026-10-09), minted in the 36-45 band.
    ///
    /// Retail stepped this event's soul gem by level: Petty 18-19, Middling 37,
    /// Common 40-43, Exceptional 47, Grand 56-58, Glorious 61-100 (21 characters).
    /// The band data gave every 16-35 character Petty and every 46+ one Glorious. The
    /// retail captures never show an event paying Greater (or Elevated) at all; at
    /// Sephoris's level retail paid Middling or Common, which is what is served now.
    /// Grand starts at 49, not 50: HauDrauf (#370) — players parked at 49 to farm
    /// Grand gems — and no capture exists at 49 to contradict him.
    #[test]
    fn report_367_soul_gems_follow_the_claimants_level() {
        let sd = static_data();
        let gld: Uuid = "26eb6ab5-2d8c-4993-820e-ada79f6f00a8".parse().unwrap();
        for (level, gem) in [
            (20, "19ce1a65-057f-4f34-a0ed-27de7c085662"),  // Petty
            (26, "19ce1a65-057f-4f34-a0ed-27de7c085662"),  // Petty
            (35, "eca5bd64-5e5d-4d0d-bfa3-b6fd427be029"),  // Middling
            (40, "1ba210b4-8cca-4f2f-b942-8fab80a52fd8"),  // Common
            (47, "3932e499-441e-4c6d-b671-9a03131ebe6f"),  // Exceptional
            (48, "3932e499-441e-4c6d-b671-9a03131ebe6f"),  // Exceptional (retail L48)
            (49, "68d7941e-8c8d-47bf-9f66-becb058f1817"),  // Grand, per HauDrauf (#370)
            (60, "68d7941e-8c8d-47bf-9f66-becb058f1817"),  // Grand
            (100, "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb"), // Glorious
        ] {
            let paid = materials_paid_at(&sd, gld, level);
            let gem: Uuid = gem.parse().unwrap();
            assert_eq!(paid.get(&gem), Some(&5), "level {level}: paid {paid:?}");
            assert_eq!(paid.len(), 1, "level {level}: {paid:?}");
        }
    }

    /// Identity test against retail: every material any retail event instance paid,
    /// at the level its character had when the instance first appears — 409 distinct
    /// (template, level, slot, item) observations, 29 templates, levels 10-100
    /// (`script/extract_event_material_ladders.py`). A row minted at that level must
    /// carry exactly that item in the same slot. 31 of them failed on the band data
    /// alone.
    #[test]
    fn report_362_every_retail_event_material_is_paid_at_its_level() {
        #[derive(serde::Deserialize)]
        struct File {
            observations: Vec<(Uuid, i64, String, Uuid)>,
        }
        let raw = include_str!("../../blades_lib/src/event_material_observations.json");
        let file: File = serde_json::from_str(raw).expect("observations parse");
        assert!(file.observations.len() >= 400, "{} observations", file.observations.len());

        let sd = static_data();
        let mut wrong = Vec::new();
        for (gld, level, slot, item) in &file.observations {
            let tmpl = &sd.event_quests.templates[gld];
            let carried = match slot.as_str() {
                "rewards" => tmpl
                    .rewards_for_level(*level)
                    .iter()
                    .any(|t| t.stackable_items.contains_key(item)),
                "final" => tmpl
                    .final_reward_for_level(*level)
                    .is_some_and(|f| f.stackable_items.contains_key(item)),
                other => panic!("slot {other}"),
            };
            if !carried {
                wrong.push(format!("{gld} level {level} {slot}: retail paid {item}"));
            }
        }
        assert!(wrong.is_empty(), "{} of {}:\n{}", wrong.len(), file.observations.len(), wrong.join("\n"));
    }

    /// No event milestone may grant gold, sigils or gems as a backpack item:
    /// 0 of 155 captured event `/complete` rewards do.
    #[test]
    fn no_event_milestone_grants_a_currency_as_an_item() {
        let sd = static_data();
        let mut checked = 0;
        for (gld, tmpl) in &sd.event_quests.templates {
            for tier in 0..tmpl.milestone_count() {
                let r = event_milestone_reward(&sd, Uuid::from_u128(0xE7), &quest(*gld), tier)
                    .expect("every tier pays");
                for id in r.stackable_items.keys() {
                    assert!(
                        !blades_lib::economy::is_currency(*id),
                        "{gld} tier {}: currency {id} granted as a stackable item",
                        tier + 1
                    );
                }
                checked += 1;
            }
        }
        assert!(checked >= 200, "checked {checked} milestones");
    }

    /// A template whose captured last tier lacks `finalReward` (a different season's
    /// capture) still pays it on top, and pays it once.
    #[test]
    fn a_final_reward_missing_from_the_captured_last_tier_is_added_once() {
        let sd = static_data();
        let mut added = 0;
        for (gld, tmpl) in &sd.event_quests.templates {
            let Some(bonus) = tmpl.final_reward.as_ref() else { continue };
            let last = tmpl.milestone_count() - 1;
            let base = tmpl.payout(last).expect("last tier");
            if reward_already_carries(&base, bonus) {
                continue;
            }
            let paid = event_milestone_reward(&sd, Uuid::from_u128(0xE7), &quest(*gld), last)
                .expect("last tier pays");
            for (id, n) in &bonus.stackable_items {
                let before = base.currencies.get(id).copied().unwrap_or(0)
                    + base.stackable_items.get(id).copied().unwrap_or(0);
                let after = paid.currencies.get(id).copied().unwrap_or(0)
                    + paid.stackable_items.get(id).copied().unwrap_or(0);
                assert_eq!(after, before + n, "{gld}: {id}");
            }
            added += 1;
        }
        assert!(added > 0, "the committed data has such templates (20 at the time of #324)");
    }

    /// An ordinary quest resolves its reward through `gldQuestId`, and every quest
    /// `quest_rewards.json` covers actually pays.
    #[test]
    fn an_ordinary_quest_pays_from_the_template_keyed_table() {
        let sd = static_data();
        let gd = game_data();
        let mut paid = 0;
        for gld in gd.quests.keys() {
            if !sd.quest_rewards.contains_key(gld) {
                continue;
            }
            let mut q = quest(*gld);
            q.r#type = blades_lib::user_data::QuestType::Normal;
            // The ROW id is deliberately not the template id, so a lookup that keys on
            // the row id instead of gldQuestId finds nothing and pays zero.
            let row_id = Uuid::from_u128(0xF00D);
            let reward = resolve_completion_reward(&sd, row_id, &q);
            assert!(
                !reward.is_empty(),
                "quest {gld} is covered but paid nothing"
            );
            paid += 1;
        }
        assert!(
            paid >= 100,
            "the committed table must cover a real share of the 185 quests, got {paid}"
        );
    }
}

/// Report #92/#98: a completed town job paid nothing.
///
/// A job's `gldQuestId` is our sentinel. It is a key in neither `quest_rewards.json`
/// nor `event_quests.json`, so `resolve_completion_reward` fell through every branch
/// to its "no captured reward — paying nothing" default. Every job finished on this
/// server since jobs existed paid zero XP and zero gold, while its own board entry
/// advertised a reward.
///
/// Ground truth is retail's own corpus, matched job-for-job rather than by shape:
/// board capture 242402 rolled job `1385706b-…` with `rewardXp: 1526`,
/// `rewardItemCount: 1000`, `rewardItemId: f8d27767-…`, and that job's `/complete`
/// (capture 242652) answered
/// `{"currencies":{"f8d27767-…":1000},"characterXp":1526,"townXp":0}`. Job
/// `9ba20667-…` from the same board declared 1586/1000 and paid 1586/1000. Gold
/// arrives under `currencies`, never `stackableItems`.
#[cfg(test)]
mod report92_job_completion_reward_tests {
    use super::*;
    use blades_lib::static_data::StaticData;
    use blades_lib::user_data::Quest;

    const GOLD: &str = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2";
    const CHAR: Uuid = Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0);
    /// 2026-05-13 Wed 06:00 UTC — the weekday whose board prod capture id=1105 shows.
    const NOW_WED: u64 = 1_778_648_400 + 3600;

    fn static_data() -> StaticData {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        crate::static_loader::load(&dir)
    }

    fn game_data() -> blades_lib::game_data::GameData {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid parsed.json")
    }

    fn job_pools() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/job_pools.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid job_pools.json")
    }

    /// The real board, rolled from the committed job pools.
    fn board() -> Vec<Value> {
        let pools = job_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (jobs, _t) = jobs_gen::generate(&pools, CHAR, 48, 0, boundary, NOW_WED);
        assert!(
            !jobs.is_empty(),
            "the committed pools must roll a board at all"
        );
        jobs
    }

    fn job_ids(jobs: &[Value]) -> std::collections::HashSet<Uuid> {
        jobs.iter()
            .map(|j| {
                Uuid::parse_str(j["questId"].as_str().expect("job has questId"))
                    .expect("questId is a uuid")
            })
            .collect()
    }

    /// One retail job, reduced to the three `jobSetup` fields the reward is built
    /// from plus the two the row needs.
    fn retail_job(quest_id: &str, xp: u64, gold: u64) -> Value {
        json!({
            "questId": quest_id,
            "version": 0,
            "difficultyLevel": 75,
            "seed": 0,
            "objectiveStatuses": {},
            "jobSetup": {
                "rewardXp": xp,
                "rewardItemId": GOLD,
                "rewardItemCount": gold,
            },
        })
    }

    /// THE identity test: our reward for a retail job must equal, key for key, the
    /// `reward` block retail's own `/complete` returned for that same job.
    #[test]
    fn a_job_pays_exactly_what_its_jobsetup_declared() {
        // capture 242402 (board) -> capture 242652 (/complete)
        let reward = jobs_gen::job_completion_reward(&retail_job(
            "1385706b-d464-4e30-838b-55558644e8ba",
            1526,
            1000,
        ));
        assert_eq!(
            serde_json::to_value(&reward).unwrap(),
            json!({ "currencies": { GOLD: 1000 }, "characterXp": 1526 }),
            "must match retail's /complete reward block for job 1385706b"
        );

        // capture 242402 (board) -> capture 243017 (/complete)
        let reward = jobs_gen::job_completion_reward(&retail_job(
            "9ba20667-fd89-4bda-a8f3-708013b70710",
            1586,
            1000,
        ));
        assert_eq!(
            serde_json::to_value(&reward).unwrap(),
            json!({ "currencies": { GOLD: 1000 }, "characterXp": 1586 }),
            "must match retail's /complete reward block for job 9ba20667"
        );
    }

    /// A zero `rewardItemCount` is a real retail value (it appears at many
    /// difficulties in the sampled boards), and retail omits `currencies` entirely
    /// rather than sending `{gold: 0}`.
    #[test]
    fn a_job_that_declares_no_gold_omits_currencies() {
        let reward = jobs_gen::job_completion_reward(&retail_job(
            "00000000-0000-4000-8000-000000000000",
            400,
            0,
        ));
        assert_eq!(
            serde_json::to_value(&reward).unwrap(),
            json!({ "characterXp": 400 }),
            "no gold means no currencies key, not a zero one"
        );
    }

    /// The end-to-end bug: roll the real board, store each job the way `/quests`
    /// does, and complete it. Before the fix every one of these paid `RewardGrant`'s
    /// default — empty — because the sentinel matched no reward table.
    #[test]
    fn every_job_on_the_board_pays_on_completion() {
        let sd = static_data();
        let gd = game_data();
        let jobs = board();
        for job in &jobs {
            let row =
                jobs_gen::job_quest_db_entry(job, CHAR, &gd, &crate::quest::shipped_scaling())
                    .expect("job row");
            let paid = resolve_completion_reward(&sd, row.id, &row.info.0);
            let declared = jobs_gen::job_completion_reward(job);
            assert!(
                !paid.is_empty(),
                "job {} completed and paid nothing",
                job["questId"]
            );
            assert_eq!(
                serde_json::to_value(&paid).unwrap(),
                serde_json::to_value(&declared).unwrap(),
                "job {} paid something other than its own jobSetup",
                job["questId"]
            );
        }

        // The control that makes the above discriminating: a story quest the reward
        // table DOES cover must still resolve through `gldQuestId`, so the new job
        // branch cannot be swallowing the ordinary path.
        let covered = gd
            .quests
            .keys()
            .find(|gld| sd.quest_rewards.contains_key(gld))
            .copied()
            .expect("the committed table covers at least one quest");
        let mut story: Quest = serde_json::from_value(json!({
            "version": 0, "type": "NORMAL", "objectiveStatuses": {},
            "difficultyLevel": 0, "seed": 0, "gldQuestId": covered, "completed": false,
        }))
        .expect("fixture quest");
        story.job_reward = None;
        let reward = resolve_completion_reward(&sd, Uuid::from_u128(0xF00D), &story);
        assert!(!reward.is_empty(), "the story-quest control must still pay");
    }

    /// Report #279: HauDrauf completed one job (`21c4b74d...`) and then had five
    /// live job rows left in storage. That job was the weekly BOSS duel, and a boss
    /// or featured pool (`maxTotal` 1) is not refilled in its window (report #306:
    /// 0 of 6 such retail completions were). So the board is exactly those five
    /// rows; the standard-pool refill is covered by
    /// `report_306_daily_job_hang::a_completed_standard_job_is_still_replaced`.
    #[test]
    fn a_completed_boss_job_leaves_the_other_five_and_no_replacement() {
        let pools = job_pools();
        let character = Uuid::parse_str("489620db-7f90-4a03-bb7c-f7e92a9c73cb").unwrap();
        let now = 1_790_661_960; // 2026-09-29 06:06 UTC
        let boundary = jobs_gen::current_reset_boundary(&pools, now);
        assert_eq!(boundary, 1_790_658_000, "the report's 05:00 UTC reset");

        let (base, _) = jobs_gen::generate(&pools, character, 100, 44, boundary, now);
        let base_ids = job_ids(&base);
        let completed = Uuid::parse_str("21c4b74d-56f2-4053-b16d-61c350f66bfd").unwrap();
        assert!(
            base_ids.contains(&completed),
            "control: the exact job HauDrauf finished must be on the original board"
        );

        let completed_ids = std::collections::HashSet::from([completed]);
        let (replenished, _) = jobs_gen::generate_replenished(
            &pools,
            character,
            100,
            44,
            boundary,
            now,
            &completed_ids,
        );
        let replenished_ids = job_ids(&replenished);

        assert_eq!(
            replenished.len(),
            base.len() - 1,
            "a completed boss job is not replaced in its window"
        );
        assert!(
            !replenished_ids.contains(&completed),
            "the completed job must be deleted from the client board"
        );
        for open in [
            "1b3fe471-d9ca-481e-b2cd-1a69d681b971",
            "41d2b7ae-bdbf-4d54-9eb9-eb1e9b21a1bb",
            "c53d368d-7f5e-4254-8853-4f9dd70fd2ff",
            "c81e5650-ee21-421d-8509-cc22202354b8",
            "eb437dde-2ac3-4adc-bb6d-d18c1d8bd113",
        ] {
            let id = Uuid::parse_str(open).unwrap();
            assert!(
                replenished_ids.contains(&id),
                "open job {id} from HauDrauf's rows should stay on the board"
            );
        }
        let replacements: Vec<_> = replenished_ids.difference(&base_ids).collect();
        assert!(replacements.is_empty(), "no replacement: {replacements:?}");
    }

    /// Negative control for the report #279 fix: an untouched board takes the old
    /// generator path and does not reshuffle ids.
    #[test]
    fn replenishment_without_completed_jobs_is_the_original_board() {
        let pools = job_pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW_WED);
        let (base, _) = jobs_gen::generate(&pools, CHAR, 48, 0, boundary, NOW_WED);
        let (same, _) = jobs_gen::generate_replenished(
            &pools,
            CHAR,
            48,
            0,
            boundary,
            NOW_WED,
            &std::collections::HashSet::new(),
        );
        assert_eq!(
            serde_json::to_value(&same).unwrap(),
            serde_json::to_value(&base).unwrap(),
            "empty exclusions must not move the board"
        );
    }

    /// Rows stored before `job_reward` existed carry `None`. They must still pay
    /// something rather than fall back into the "paying nothing" default — the board
    /// re-rolls daily, so this path is temporary, but "temporary" here meant a
    /// player's finished job.
    #[test]
    fn a_job_row_from_before_this_field_still_pays_xp() {
        let sd = static_data();
        let legacy: Quest = serde_json::from_value(json!({
            "version": 0, "type": "NORMAL", "objectiveStatuses": {},
            "difficultyLevel": 75, "seed": 0,
            "gldQuestId": jobs_gen::JOB_SENTINEL_GLD, "completed": false,
        }))
        .expect("a pre-field row must still deserialize");
        assert!(
            legacy.job_reward.is_none(),
            "the fixture is the legacy shape"
        );

        let reward = resolve_completion_reward(&sd, Uuid::from_u128(1), &legacy);
        // Retail's job XP for difficulty 75 (#337): 45 of 45 retail difficulty-75 jobs
        // declared `rewardXp: 1526`, and job `1385706b-…`'s `/complete` paid exactly
        // that. This path used to pay ONE enemy's kill XP (261) — a sixth of it.
        assert_eq!(
            reward.character_xp, 1526,
            "the legacy path pays retail's job xp for the row's difficulty"
        );
        assert_ne!(
            reward.character_xp,
            sd.quests_daily.level_scaling.given_xp(75),
            "and not the kill xp of a single enemy, or this proves nothing"
        );
        assert!(
            reward.currencies.is_empty(),
            "and no gold, which is unrecoverable"
        );
    }

    /// `job_reward` is ours, not retail's: no captured quest carries the key, and
    /// job rows are kept out of `quests[]` anyway. It must never appear on the wire
    /// for a quest that does reach the client.
    #[test]
    fn job_reward_is_absent_from_a_non_job_quests_wire() {
        let story: Quest = serde_json::from_value(json!({
            "version": 0, "type": "NORMAL", "objectiveStatuses": {},
            "difficultyLevel": 0, "seed": 0,
            "gldQuestId": Uuid::from_u128(0xABC), "completed": false,
        }))
        .expect("fixture quest");
        let wire = serde_json::to_value(&story).unwrap();
        assert!(
            wire.get("jobReward").is_none(),
            "jobReward leaked onto a story quest's wire: {wire}"
        );

        // Control: the field really does serialize when it is set, so the assertion
        // above is about `skip_serializing_if` and not about a field that never works.
        let mut job = story;
        job.job_reward = Some(blades_lib::economy::RewardGrant {
            character_xp: 7,
            ..Default::default()
        });
        assert_eq!(
            serde_json::to_value(&job).unwrap()["jobReward"]["characterXp"],
            json!(7)
        );
    }
}

#[cfg(test)]
mod finished_quest_tests {
    use super::*;
    use blades_lib::user_data::{Quest, QuestType};

    /// A quest body built through serde from the exact key set production stores.
    fn quest(gld: Uuid, r#type: &str, completed: bool) -> Quest {
        serde_json::from_value(json!({
            "version": 0,
            "type": r#type,
            "objectiveStatuses": {},
            "difficultyLevel": 0,
            "seed": 0,
            "gldQuestId": gld,
            "completed": completed,
        }))
        .expect("fixture quest must deserialize")
    }

    /// "Rescuing the Townsfolk" — 8 of the 11 rows stuck in prod were this one.
    const MQ03: Uuid = Uuid::from_u128(0x5AD30483_8994_484E_B6DC_A5E9014CC4D5_u128);

    /// The measurement this fix rests on.
    ///
    /// 719 captured retail `/quests` bodies carry a `quests` array holding 563 entries.
    /// Every one of those entries has a `completed` key, and every one of them is
    /// `false` — so retail never lists a quest the player has finished. Our
    /// `/complete` left the row in place, and the client went on offering it.
    #[test]
    fn a_completed_ordinary_quest_is_stale() {
        assert!(
            is_finished_ordinary_quest(&quest(MQ03, "NORMAL", true)),
            "a finished main quest must be retired from the quest log"
        );
    }

    /// The control: the same predicate must NOT fire on the live entries, or it would
    /// delete the player's whole quest log instead of just the finished rows.
    #[test]
    fn an_unfinished_ordinary_quest_is_kept() {
        assert!(!is_finished_ordinary_quest(&quest(MQ03, "NORMAL", false)));
    }

    /// The row that survives until the next board refresh must not pay twice.
    ///
    /// `/quests` prunes finished rows, but only on the next refresh — until then the
    /// row is still loadable, and before this guard a second `/complete` on it
    /// incremented `completedQuests` and paid the reward again. One prod character
    /// had reached a count of 4 on a quest the APK permits once.
    #[test]
    fn completing_the_same_stored_quest_twice_pays_once() {
        let mut info = quest(MQ03, "NORMAL", false);
        assert!(
            mark_quest_completed_once(&mut info),
            "the first completion applies and is paid"
        );
        assert!(info.completed, "and is recorded on the row");
        assert!(
            !mark_quest_completed_once(&mut info),
            "a retry must not count or reward the quest a second time"
        );
        assert!(info.completed, "the row still reads as completed");
    }

    /// Jobs and events have their own lifecycles — rotated on the daily reset and
    /// retired when the window closes — and both are pruned elsewhere in `/quests`.
    /// Claiming them here would delete an event row mid-window.
    #[test]
    fn jobs_and_events_are_left_to_their_own_prunes() {
        let job = quest(jobs_gen::JOB_SENTINEL_GLD, "NORMAL", true);
        assert!(
            jobs_gen::is_job_row(&job),
            "fixture must actually be a job row"
        );
        assert!(!is_finished_ordinary_quest(&job));

        let event = quest(MQ03, "GAME_EVENT", true);
        assert!(matches!(event.r#type, QuestType::GameEvent));
        assert!(!is_finished_ordinary_quest(&event));
    }

    /// `split_quest_rows` is what builds the wire `quests[]`. Even with the prune in
    /// place, a completed row that somehow survives must never reach the client —
    /// this is the assertion that pins the retail shape end to end.
    #[test]
    fn a_completed_row_never_reaches_the_wire() {
        let generated: blades_lib::user_data::DungeonGeneratedData =
            serde_json::from_value(json!({ "algorithmVersion": 0, "version": 0 }))
                .expect("minimal generated data must deserialize");
        let rows = vec![
            (MQ03, quest(MQ03, "NORMAL", true), Some(generated.clone())),
            (
                Uuid::from_u128(7),
                quest(Uuid::from_u128(7), "NORMAL", false),
                Some(generated),
            ),
        ];
        let live: Vec<_> = rows
            .into_iter()
            .filter(|(_, info, _)| !is_finished_ordinary_quest(info))
            .collect();
        let (quests, _events, _generated) =
            split_quest_rows(live.into_iter(), &std::collections::HashSet::new());
        assert_eq!(quests.len(), 1, "only the unfinished quest is listed");
        assert!(
            quests.iter().all(|q| !q.quest.completed),
            "retail serves completed:false on all 563 captured entries"
        );
    }
}

#[cfg(test)]
mod deleted_quest_ids_tests {
    use super::GetQuestsResponse;

    /// Retail sends `deletedQuestIds` in 17.91% of captured `/quests` responses and
    /// NEVER sends it empty — it is a "there were deletions" signal, not a list that
    /// is always present. So an empty one must be omitted, not serialized as `[]`.
    #[test]
    fn an_empty_deletion_list_is_omitted_entirely() {
        let json = serde_json::to_value(GetQuestsResponse {
            quests: vec![],
            dungeon_generated_data_list: vec![],
            jobs: vec![],
            character: blades_lib::user_data::CompleteCharacterWithIdWithoutData {
                id: uuid::Uuid::nil(),
                character: Default::default(),
            },
            job_pools: serde_json::json!([]),
            deleted_quest_ids: vec![],
            game_event_quests: vec![],
            game_event_quests_in_warning: vec![],
            game_event_quests_finished: vec![],
        })
        .unwrap();
        assert!(
            json.get("deletedQuestIds").is_none(),
            "retail never sends an empty deletedQuestIds; got {json}",
        );
    }

    /// ...and when something WAS deleted, it must be present and camelCased. This is
    /// the control: a change that skipped the field unconditionally would pass the
    /// test above and fail this one.
    #[test]
    fn a_non_empty_deletion_list_is_sent() {
        let id = uuid::Uuid::parse_str("159bc1e7-454c-4e2a-90cf-e200c74b961a").unwrap();
        let json = serde_json::to_value(GetQuestsResponse {
            quests: vec![],
            dungeon_generated_data_list: vec![],
            jobs: vec![],
            character: blades_lib::user_data::CompleteCharacterWithIdWithoutData {
                id: uuid::Uuid::nil(),
                character: Default::default(),
            },
            job_pools: serde_json::json!([]),
            deleted_quest_ids: vec![id],
            game_event_quests: vec![],
            game_event_quests_in_warning: vec![],
            game_event_quests_finished: vec![],
        })
        .unwrap();
        assert_eq!(
            json["deletedQuestIds"],
            serde_json::json!(["159bc1e7-454c-4e2a-90cf-e200c74b961a"]),
            "a real deletion must reach the client",
        );
    }
}

#[cfg(test)]
mod report62_quest_map_tests {
    use super::*;
    use blades_lib::user_data::Quest;

    /// Built through serde from the exact key set production stores, so the fixture
    /// tracks the wire shape rather than restating the Rust struct.
    fn quest(gld: Uuid) -> Quest {
        serde_json::from_value(json!({
            "version": 0,
            "type": "NORMAL",
            "objectiveStatuses": {},
            "difficultyLevel": 0,
            "seed": 0,
            "gldQuestId": gld,
            "completed": false,
        }))
        .expect("fixture quest must deserialize")
    }

    fn generated() -> blades_lib::user_data::DungeonGeneratedData {
        serde_json::from_value(json!({ "algorithmVersion": 0, "version": 0 }))
            .expect("minimal generated data must deserialize")
    }

    /// THE invariant the client relies on: every quest it is told about has a
    /// matching `generatedData` entry. Break it and the quest map waits forever for
    /// data that never arrives — which is exactly report #62.
    #[test]
    fn every_advertised_quest_has_generated_data() {
        let with_data = Uuid::from_u128(1);
        let without_data = Uuid::from_u128(2);
        let normal = Uuid::from_u128(0xAAAA);

        let (quests, _events, data) = split_quest_rows(
            vec![
                (with_data, quest(normal), Some(generated())),
                // "The Message": a real quest whose template ships no dungeon, so
                // `generate_quest_data` produced nothing.
                (without_data, quest(normal), None),
            ]
            .into_iter(),
            &Default::default(),
        );

        assert_eq!(quests.len(), 1, "the dataless quest must not be advertised");
        assert_eq!(quests[0].quest_id, with_data);

        let advertised: Vec<Uuid> = quests.iter().map(|q| q.quest_id).collect();
        let have_data: Vec<Uuid> = data.iter().map(|g| g.quest_id).collect();
        assert_eq!(
            advertised, have_data,
            "every advertised quest must have generated data, keyed by the same id"
        );
    }

    /// The control for the test above: a quest WITH data is still served. Without
    /// this, a `split_quest_rows` that returned two empty vecs would pass.
    #[test]
    fn a_quest_with_data_is_still_served() {
        let id = Uuid::from_u128(7);
        let (quests, _events, data) = split_quest_rows(
            vec![(id, quest(Uuid::from_u128(0xBBBB)), Some(generated()))].into_iter(),
            &Default::default(),
        );
        assert_eq!(quests.len(), 1, "a normal quest must still be served");
        assert_eq!(data.len(), 1, "…with its generated data");
        assert_eq!(quests[0].quest_id, id);
    }

    /// Job rows stay out of `quests[]` — they are surfaced only in `jobs[]`, and the
    /// two arrays never overlap in prod. This behaviour predates the fix and must
    /// survive it.
    #[test]
    fn job_rows_are_still_excluded() {
        let (quests, _events, data) = split_quest_rows(
            vec![(
                Uuid::from_u128(9),
                quest(jobs_gen::JOB_SENTINEL_GLD),
                Some(generated()),
            )]
            .into_iter(),
            &Default::default(),
        );
        assert!(quests.is_empty(), "a job row must not appear in quests[]");
        assert!(data.is_empty());
    }

    /// A character holding a mix — the shape the two level-48 Adventurers actually
    /// have in production: several playable quests, six job rows, and one dataless
    /// quest. Only the playable ones survive, and the arrays stay aligned.
    #[test]
    fn the_production_shape_resolves_to_a_consistent_pair_of_arrays() {
        let mut rows = Vec::new();
        for i in 0..3u128 {
            rows.push((
                Uuid::from_u128(100 + i),
                quest(Uuid::from_u128(0xC0 + i)),
                Some(generated()),
            ));
        }
        for i in 0..6u128 {
            rows.push((
                Uuid::from_u128(200 + i),
                quest(jobs_gen::JOB_SENTINEL_GLD),
                None,
            ));
        }
        // "The Message"
        rows.push((Uuid::from_u128(300), quest(Uuid::from_u128(0xCCA4)), None));

        let (quests, _events, data) = split_quest_rows(rows.into_iter(), &Default::default());
        assert_eq!(quests.len(), 3, "three playable quests");
        assert_eq!(data.len(), 3, "each with its data");
        let a: Vec<Uuid> = quests.iter().map(|q| q.quest_id).collect();
        let b: Vec<Uuid> = data.iter().map(|g| g.quest_id).collect();
        assert_eq!(a, b);
    }
}

#[cfg(test)]
mod objectives_wire_tests {
    use super::*;

    /// The live 400.
    ///
    /// 137 of the 139 `/objectives` calls ever made against this server answered
    /// `400 Json deserialize error: unknown variant 'Completed', expected 'Active'`
    /// — the body below, verbatim from capture 257672. `QuestStatus` modelled only
    /// `Active`, so the request could not be parsed and no quest could ever be
    /// finished through the normal flow. It is the reason the audit's answer to
    /// "how many quests are playable end to end" was zero.
    #[test]
    fn the_client_report_that_400ed_every_time_now_parses() {
        let body = r#"{"objectiveUpdates":{"76b97069-67e9-4202-aa93-8bc1dc7fbc65":{"status":"Completed","progress":1.0}}}"#;
        let parsed: ObjectivesRequest = serde_json::from_str(body)
            .expect("the client's own completion report must deserialize");
        let id = Uuid::parse_str("76b97069-67e9-4202-aa93-8bc1dc7fbc65").unwrap();
        let update = parsed
            .objective_updates
            .get(&id)
            .expect("the objective is there");
        assert!(
            matches!(update.status, blades_lib::user_data::QuestStatus::Completed),
            "a completion report must arrive as Completed"
        );
        assert_eq!(update.progress, 1.0);
    }

    /// The control: an in-progress report still parses, so the fix is a widening and
    /// not a swap. Without this a build that renamed `Active` to `Completed` would
    /// pass the test above.
    #[test]
    fn an_in_progress_report_still_parses() {
        let body = r#"{"objectiveUpdates":{"76b97069-67e9-4202-aa93-8bc1dc7fbc65":{"status":"Active","progress":0.5}}}"#;
        let parsed: ObjectivesRequest = serde_json::from_str(body).expect("still valid");
        let id = Uuid::parse_str("76b97069-67e9-4202-aa93-8bc1dc7fbc65").unwrap();
        assert!(matches!(
            parsed.objective_updates[&id].status,
            blades_lib::user_data::QuestStatus::Active
        ));
    }

    /// The three response shapes retail actually sent, and the key each quest comes
    /// back under. An event quest answers under `gameEventQuest` with no reward; an
    /// ordinary one under `quest`.
    #[test]
    fn an_event_quest_answers_under_its_own_key_and_pays_nothing_here() {
        let q = QuestWithId {
            quest_id: Uuid::from_u128(1),
            quest: serde_json::from_value(json!({
                "version": 1, "type": "GAME_EVENT", "objectiveStatuses": {},
                "difficultyLevel": 10, "seed": 0,
                "gldQuestId": "7f0d1508-312b-4036-970f-ff5f4c342526",
                "completed": false,
            }))
            .unwrap(),
        };
        let (quest, game_event_quest) = objectives_wire_slot(true, q);
        let wire = serde_json::to_value(ObjectivesResponse {
            reward: RewardGrant::default(),
            inventory: None,
            character: None,
            quest,
            game_event_quest,
        })
        .unwrap();
        assert!(
            wire.get("quest").is_none(),
            "an event quest is not under `quest`"
        );
        assert_eq!(wire["gameEventQuest"]["type"], "GAME_EVENT");
        assert!(
            wire.get("reward").is_none(),
            "no reward on an event objective"
        );
        assert_eq!(
            wire.as_object().unwrap().len(),
            1,
            "retail's 363 event responses were the quest and nothing else: {wire}"
        );
    }

    /// ...and the ordinary case keeps its `quest` key, so the split is a routing
    /// decision rather than a rename.
    #[test]
    fn an_ordinary_quest_still_answers_under_quest() {
        let q = QuestWithId {
            quest_id: Uuid::from_u128(2),
            quest: serde_json::from_value(json!({
                "version": 1, "type": "NORMAL", "objectiveStatuses": {},
                "difficultyLevel": 10, "seed": 0,
                "gldQuestId": "7f0d1508-312b-4036-970f-ff5f4c342526",
                "completed": false,
            }))
            .unwrap(),
        };
        let (quest, game_event_quest) = objectives_wire_slot(false, q);
        let wire = serde_json::to_value(ObjectivesResponse {
            reward: RewardGrant::default(),
            inventory: None,
            character: None,
            quest,
            game_event_quest,
        })
        .unwrap();
        assert_eq!(wire["quest"]["type"], "NORMAL");
        assert!(wire.get("gameEventQuest").is_none());
    }

    /// A `GAME_EVENT` quest must survive a round-trip through the wire with its
    /// event fields intact — they are what the client needs to show the milestone
    /// track, and they are stored in the same JSONB the row is read back from.
    #[test]
    fn the_event_fields_round_trip_and_stay_off_an_ordinary_quest() {
        let event: blades_lib::user_data::Quest = serde_json::from_value(json!({
            "version": 1, "type": "GAME_EVENT", "objectiveStatuses": {},
            "difficultyLevel": 10, "seed": -270074008i64,
            "gldQuestId": "7f07d85f-f4ed-4762-b670-79e36b224902",
            "gameEventQuestData": { "gameEventInstanceId": "ffcbe281-e953-49c9-b048-69780616c034::1777694400" },
            "rewards": [{ "stackableItems": { "c64bcb53-41f4-41ba-892a-fe2cca423caa": 1 }, "characterXp": 700 }],
            "finalReward": { "stackableItems": { "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2": 25000 } },
            "completed": false,
        }))
        .expect("a captured event quest must deserialize");
        let back = serde_json::to_value(&event).unwrap();
        assert_eq!(
            back["gameEventQuestData"]["gameEventInstanceId"],
            "ffcbe281-e953-49c9-b048-69780616c034::1777694400"
        );
        assert_eq!(back["rewards"][0]["characterXp"], 700);
        assert_eq!(
            back["finalReward"]["stackableItems"]["f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2"],
            25000
        );

        // An ordinary quest must not grow the three event keys — retail never sent
        // them on a NORMAL quest, and a `null` there is a wire change.
        let normal: blades_lib::user_data::Quest = serde_json::from_value(json!({
            "version": 1, "type": "NORMAL", "objectiveStatuses": {},
            "difficultyLevel": 10, "seed": 0,
            "gldQuestId": "7f0d1508-312b-4036-970f-ff5f4c342526", "completed": false,
        }))
        .unwrap();
        let back = serde_json::to_value(&normal).unwrap();
        for key in ["gameEventQuestData", "rewards", "finalReward"] {
            assert!(back.get(key).is_none(), "{key} must be omitted, got {back}");
        }
    }
}

#[cfg(test)]
mod playability_sweep {
    use super::*;
    use blades_lib::static_data::StaticData;
    use blades_lib::util::quest::generate_quest_data;

    fn static_data() -> StaticData {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        crate::static_loader::load(&dir)
    }

    fn game_data() -> blades_lib::game_data::GameData {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static/parsed.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid parsed.json")
    }

    /// The audit, as a test: walk every shipped quest through the accept path at a
    /// spread of player levels and report what actually resolves.
    ///
    /// Accepting is where quest data has historically blown up — a nil dungeon uuid
    /// used to `.ok_or(DungeonNotFound)` and 500 the six dialogue quests, and a
    /// malformed item spawn used to panic. So this asserts three things, and the
    /// numbers are stated rather than implied so a regression reads as a diff:
    ///
    ///  * every one of the 185 quests generates a body without erroring;
    ///  * exactly the 19 nil-dungeon quests come back without dungeon data, and every
    ///    other quest comes back WITH it;
    ///  * every quest with a dungeon has at least one objective, because a quest with
    ///    no objective cannot be completed by the client.
    #[test]
    fn every_shipped_quest_generates_at_every_level() {
        let sd = static_data();
        let gd = game_data();
        let scaling = &sd.quests_daily.level_scaling;

        let mut errored = Vec::new();
        let mut no_dungeon = Vec::new();
        let mut no_objectives: Vec<Uuid> = Vec::new();
        let total = gd.quests.len();

        for quest_id in gd.quests.keys() {
            for level in [1i64, 15, 48, 86, 100] {
                match generate_quest_data(&gd, *quest_id, level, scaling) {
                    Err(e) => errored.push(format!("{quest_id} @ {level}: {e}")),
                    Ok((quest, dungeon)) => {
                        if level != 1 {
                            continue; // the shape checks below do not vary with level
                        }
                        if dungeon.is_none() {
                            no_dungeon.push(*quest_id);
                        } else if quest.objective_statuses.is_empty() {
                            no_objectives.push(*quest_id);
                        }
                    }
                }
            }
        }

        assert_eq!(total, 185, "the shipped quest corpus is 185 quests");
        assert!(
            errored.is_empty(),
            "{} quest(s) failed to generate:\n{}",
            errored.len(),
            errored.join("\n")
        );
        assert_eq!(
            no_dungeon.len(),
            19,
            "exactly the 19 nil-dungeon quests have no dungeon; got {}: {:?}",
            no_dungeon.len(),
            no_dungeon
        );
        // Every nil-dungeon quest must be one quests_daily.json already knows about,
        // so the two never drift apart.
        let declared = sd.quests_daily.non_dungeon_ids();
        for id in &no_dungeon {
            assert!(
                declared.contains(id),
                "{id} has no dungeon but is not in nonDungeonQuests"
            );
        }

        // Report #117: the extractor used to read only DungeonQuestHolder assets,
        // silently omitting every GenericQuestHolder. Pin the complete APK-derived
        // set so fixing MQ04 alone cannot leave the other thirteen broken.
        let generic_quest_ids = [
            "095b8109-0e57-400d-bcf4-70e10df8cdbf",
            "11bbd032-4451-48fd-9c00-ec6438f14f04",
            "1709a8bc-79dd-4a7f-b7f8-3a6022646dda",
            "3b478dfa-73bb-42df-a420-05cf83d015bc",
            "3e382c0a-0ccb-4009-9d24-d168f9fe92a1",
            "49e0b072-ffd7-44e7-8d72-8f001d1a079f",
            "51e41844-7c98-44a6-802c-05b7e0bed137",
            "60f1aaae-2ac1-4fa9-ad17-78a24452ffc6",
            "6e9e8b45-20ce-4004-bc49-7ba81de18b0e",
            "77c752cb-c7cb-4ad9-b8d6-dae7064540d0",
            "7db39937-ad5d-49ab-9d43-0379f14848de",
            "b7f4d378-566b-45eb-9bda-206911901821",
            "cca4a80b-96f1-4b5c-82b8-9d4964c7f44a",
            "e46ddb4e-6d61-498b-bff7-ed83c4053e71",
        ];
        for raw in generic_quest_ids {
            let id = Uuid::parse_str(raw).unwrap();
            assert!(gd.quests.contains_key(&id), "generic quest {id} is missing");
        }

        // MQ04 is the reporter's direct reproduction and must retain both objectives.
        let rebuild_town_hall = Uuid::parse_str("3b478dfa-73bb-42df-a420-05cf83d015bc").unwrap();
        let (quest, dungeon) = generate_quest_data(&gd, rebuild_town_hall, 1, scaling)
            .expect("MQ04 Rebuilding the Town Hall must resolve");
        assert!(dungeon.is_none(), "MQ04 is a town objective, not a dungeon");
        assert_eq!(
            quest.objective_statuses.len(),
            2,
            "both retail objectives are present"
        );
        // One known exception, and it is not a real quest: `MultiKitTest`
        // (category "test", `version: 0`) is a developer fixture the client ships. It
        // has a dungeon and zero objectives, so nothing can complete it — but nothing
        // advertises it either, and inventing an objective for it would be exactly the
        // kind of fabricated data this corpus is supposed to be free of. Pinned by id
        // so a SECOND objective-less quest, which would be a real extraction bug,
        // still fails this test.
        let multikit_test = Uuid::parse_str("7fd324c5-cfcf-42df-8db7-07651a9a8ac2").unwrap();
        no_objectives.retain(|id| *id != multikit_test);
        assert!(
            no_objectives.is_empty(),
            "{} quest(s) have a dungeon but no objectives, so the client can never \
             finish them: {:?}",
            no_objectives.len(),
            no_objectives
        );
    }

    /// Reward coverage, stated as a number so it can only move deliberately.
    ///
    /// Before the capture re-extraction this was 26/171 (15 %), because the table was
    /// keyed by the instance ids that appeared in captured `/complete` URLs rather
    /// than by the template. Resolving those back through `gldQuestId` lifted it to
    /// 103 flat / 142 covered.
    ///
    /// It then moved again, to 115 / 154, and NOT by inventing anything: the 29
    /// quests no capture ever completed were never missing a reward. The shipped
    /// quest asset carries `reward_preview`, and it IS the reward —
    /// `characterXp == reward_preview.experience` and
    /// `townXp == reward_preview.town_points` held 59/59 with one distinct ratio
    /// (exactly 1.0) across every quest where a capture and a definition both
    /// exist. `scripts/build-quest-rewards-static.py` in the capture repo
    /// generates the table from it.
    ///
    /// What is left is not a coverage gap of the same kind: 57 quests ship an
    /// explicit `0.0`, which is retail saying "this pays nothing". Every quest in
    /// the asset has a preview, so there is no missing-vs-zero ambiguity — and
    /// they are deliberately absent from the table rather than present-and-empty,
    /// because a key that pays nothing would make `contains_key` claim a coverage
    /// the player never sees.
    ///
    /// Gold is still capture-only: `reward_preview` has no currency of any kind.
    #[test]
    fn reward_coverage_is_what_the_captures_support() {
        let sd = static_data();
        let gd = game_data();
        let flat = gd
            .quests
            .keys()
            .filter(|q| sd.quest_rewards.contains_key(q))
            .count();
        let evented = gd
            .quests
            .keys()
            .filter(|q| sd.event_quests.templates.contains_key(q))
            .count();
        let covered: std::collections::HashSet<_> = gd
            .quests
            .keys()
            .filter(|q| {
                sd.quest_rewards.contains_key(q) || sd.event_quests.templates.contains_key(q)
            })
            .collect();

        assert_eq!(flat, 128, "flat rewards from quest_rewards.json");
        // 39 capture-derived ladders + the seven authored retired events (#189).
        assert_eq!(evented, 46, "milestone ladders from event_quests.json");
        assert_eq!(covered.len(), 174, "174 of 185 quests pay something");
        assert!(
            covered.len() as f64 / gd.quests.len() as f64 > 0.80,
            "coverage must stay above 80%"
        );
        // …and the two tables must not overlap: a quest is EITHER flat or a ladder.
        // Overlap would mean an event quest also has a fixed reward, and whichever
        // branch ran first would silently win.
        assert_eq!(
            flat + evented,
            covered.len(),
            "a quest must not appear in both quest_rewards.json and event_quests.json"
        );
    }

    /// Every event template ships a complete, usable ladder — five wire tiers, five
    /// granting tiers and a final reward. A partially-extracted file would otherwise
    /// pay `None` somewhere in the middle of a player's run.
    #[test]
    fn every_event_template_ships_a_complete_milestone_ladder() {
        let sd = static_data();
        assert_eq!(sd.event_quests.templates.len(), 46);
        for (gld, tmpl) in &sd.event_quests.templates {
            assert_eq!(tmpl.rewards.len(), 5, "{gld}: five wire milestones");
            assert_eq!(
                tmpl.payable_rewards.len(),
                5,
                "{gld}: five granting milestones"
            );
            assert!(tmpl.final_reward.is_some(), "{gld}: a final reward");
            assert!(!tmpl.objective_ids.is_empty(), "{gld}: objective ids");
            for step in 0..5 {
                let payout = tmpl
                    .payout(step)
                    .unwrap_or_else(|| panic!("{gld}: no tier {step}"));
                assert!(!payout.is_empty(), "{gld}: tier {step} pays nothing");
            }
            assert!(tmpl.payout(5).is_none(), "{gld}: exhausted after five");
        }
    }

    /// Every event in the calendar has a template AND a quest definition. An event
    /// with no template would advertise a quest that pays nothing; one with no
    /// definition would advertise a quest with no dungeon, which hangs the map.
    #[test]
    fn every_calendar_event_is_fully_backed() {
        let sd = static_data();
        let gd = game_data();
        assert_eq!(
            sd.game_events.iter().filter(|d| !d.preview).count(),
            46,
            "the committed calendar"
        );
        let rotating = sd
            .game_events
            .iter()
            .filter(|d| !d.annual && !d.preview)
            .count();
        for def in &sd.game_events {
            assert!(
                gd.quests.contains_key(&def.quest_id),
                "event {} points at quest {} which is not in parsed.json",
                def.event_id,
                def.quest_id
            );
            assert!(
                sd.event_quests.templates.contains_key(&def.quest_id),
                "event {} has no milestone table",
                def.event_id
            );
            if def.preview {
                // A one-off preview window (#189): not retail's schedule, and
                // self-expiring — `recurrenceInterval: 0` means it never reopens.
                assert_eq!(
                    def.recurrence.recurrence_interval, 0,
                    "a preview must be one-shot or it becomes a permanent event"
                );
            } else if def.annual {
                // The holiday events (#189) are ours: a year apart, and open for a
                // holiday's length rather than the rotation's two days.
                assert_eq!(
                    def.recurrence.recurrence_interval, 365,
                    "an annual event must advertise a 365-day period to the client"
                );
                assert!(
                    def.window_secs() >= 8 * 86_400,
                    "a holiday window of {}s is shorter than the rotation's own",
                    def.window_secs()
                );
            } else {
                // Retail ran 39 events on a 39-day cycle — one opening per day,
                // each open for two, so exactly two were ever open. The number
                // that matters is that one opens per day, not 39: with the five
                // retired events restored (#189) the library is 44 on a 44-day
                // cycle and the behaviour players saw is unchanged. Measured in
                // `the_rotation_keeps_two_open_and_one_about_to_open_every_day`.
                assert_eq!(
                    def.recurrence.recurrence_interval as usize, rotating,
                    "the cycle must be as many days as there are rotating events"
                );
                assert_eq!(def.window_secs(), 172_800, "…and stays open for two");
            }
        }
    }
}

#[cfg(test)]
mod assert_reachability {
    /// Is `assert!(request.is_none())` in `get_quests` / `accept_quest`
    /// reachable from the wire?
    ///
    /// `Json<Option<()>>` only ever yields `None`: `null` deserializes to
    /// `None` (Option wins over the unit type) and every other body is a serde
    /// error, which actix turns into a 400 before the handler runs. So the
    /// assert cannot fire and is dead — worth knowing, because a panicking
    /// assert in a request handler WOULD be a denial-of-service if it were
    /// reachable.
    #[test]
    fn option_unit_can_never_be_some() {
        assert_eq!(serde_json::from_str::<Option<()>>("null").unwrap(), None);
        for body in ["{}", "[]", "\"x\"", "1", "true", "{\"a\":1}"] {
            assert!(
                serde_json::from_str::<Option<()>>(body).is_err(),
                "{body} must be a 400 from serde, not a body the handler sees"
            );
        }
    }
}

/// The dynamicElements retail put on job names and descriptions.
///
/// Mined from 2,149 distinct job entries in 1,725 captured `/quests` responses;
/// the shapes below are what retail actually sent, per jobType. Before this the
/// server sent empty lists for every job (tracker #96, #99).
#[cfg(test)]
mod job_dynamic_elements {
    use super::jobs_gen;
    use serde_json::Value;

    fn board() -> Vec<Value> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        let pools: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("job_pools.json")).unwrap())
                .unwrap();
        let now = 1_777_800_000u64;
        let boundary = jobs_gen::current_reset_boundary(&pools, now);
        // Several characters, so every jobType is exercised rather than whichever
        // one character happened to roll.
        let mut all = Vec::new();
        for seed in 1..=24u128 {
            let (jobs, _) =
                jobs_gen::generate(&pools, uuid::Uuid::from_u128(seed), 50, 0, boundary, now);
            all.extend(jobs);
        }
        all
    }

    fn elems<'a>(job: &'a Value, field: &str) -> &'a Vec<Value> {
        job["jobSetup"][field]["dynamicElements"]
            .as_array()
            .unwrap()
    }
    fn types(job: &Value, field: &str) -> Vec<String> {
        elems(job, field)
            .iter()
            .map(|e| e["type"].as_str().unwrap_or("?").to_string())
            .collect()
    }

    /// The shape per jobType, exactly as retail sent it, on every job.
    ///
    /// An EMPTY list used to be accepted here as a faithful fallback. It is not:
    /// the client formats every job name unguarded, so an empty list throws and
    /// the quest map spins (2026-09-25). A HALF-filled list is just as fatal.
    #[test]
    fn element_shapes_match_retail_per_job_type() {
        let jobs = board();
        assert!(
            !jobs.is_empty(),
            "no jobs generated — the test would prove nothing"
        );
        let mut seen: std::collections::BTreeSet<i64> = Default::default();
        let mut populated = 0usize;

        for j in &jobs {
            let jt = j["jobSetup"]["jobType"].as_i64().unwrap();
            let (want_name, want_desc): (Vec<&str>, Vec<&str>) = match jt {
                0 => (
                    vec!["LOCALIZATION_ID"; 3],
                    vec!["INTEGER", "LOCALIZATION_ID"],
                ),
                1 => (vec!["LOCALIZATION_ID"], vec!["LOCALIZATION_ID"]),
                3 => (vec!["LOCALIZATION_ID"], vec!["INTEGER"]),
                4 => (
                    vec!["LOCALIZATION_ID", "INTEGER", "LOCALIZATION_ID"],
                    vec!["LOCALIZATION_ID", "INTEGER", "LOCALIZATION_ID"],
                ),
                5 => (vec!["LOCALIZATION_ID"; 2], vec!["LOCALIZATION_ID"]),
                other => panic!("jobType {other} is not one retail ever sent (0,1,3,4,5)"),
            };
            seen.insert(jt);

            let (gn, gd) = (types(j, "questName"), types(j, "questDescription"));
            if gn.is_empty() && gd.is_empty() {
                continue; // counted below: the ratio must be 100%
            }
            populated += 1;
            assert_eq!(gn, want_name, "jobType {jt} questName shape");
            assert_eq!(gd, want_desc, "jobType {jt} questDescription shape");
        }

        // Controls. Without these the test passes on a build that populates
        // nothing at all — which is exactly the bug it exists to catch.
        assert_eq!(
            seen.iter().copied().collect::<Vec<_>>(),
            vec![0, 1, 3, 4, 5],
            "every retail jobType must be covered by the sample"
        );
        // Every job, not most: an empty name list throws in the client's
        // String.Format and wedges the whole quest map (`quest_map_wedge_2026_09_25`).
        assert_eq!(
            populated,
            jobs.len(),
            "{} of {} jobs carried no elements; each one spins the quest map",
            jobs.len() - populated,
            jobs.len()
        );
    }

    /// The integers are not decorative — retail's value equals a field of the
    /// same jobSetup, and a plausible-looking wrong number is the failure mode
    /// this guards.
    #[test]
    fn integers_equal_the_setup_field_retail_used() {
        for j in board() {
            let s = &j["jobSetup"];
            let jt = s["jobType"].as_i64().unwrap();
            let from = |f: &str| s[f].as_i64().unwrap_or(-1);
            // Jobs that fell back to empty carry no integer to check.
            if elems(&j, "questName").is_empty() && elems(&j, "questDescription").is_empty() {
                continue;
            }
            match jt {
                0 => assert_eq!(
                    elems(&j, "questDescription")[0]["intValue"]
                        .as_i64()
                        .unwrap(),
                    from("primaryEnemyCount"),
                    "Defeat description counts primaryEnemyCount"
                ),
                3 => assert_eq!(
                    elems(&j, "questDescription")[0]["intValue"]
                        .as_i64()
                        .unwrap(),
                    from("rescueNpcCount"),
                    "Rescue description counts rescueNpcCount"
                ),
                4 => assert_eq!(
                    elems(&j, "questName")[1]["intValue"].as_i64().unwrap(),
                    from("gatherItemCount"),
                    "Gather name counts gatherItemCount"
                ),
                _ => {}
            }
        }
    }

    /// Every localization key we emit must be one retail actually used. A
    /// well-shaped list of invented keys renders as blank text in game.
    #[test]
    fn every_localization_key_was_used_by_retail() {
        let raw: Value =
            serde_json::from_str(include_str!("job_localization.json")).expect("table parses");
        let mut known: std::collections::HashSet<String> = Default::default();
        for (_, d) in raw["dungeons"].as_object().unwrap() {
            if let Some(k) = d["kitName"].as_str() {
                known.insert(k.into());
            }
            for l in d["locations"].as_array().unwrap() {
                known.insert(l.as_str().unwrap().into());
            }
        }
        for (_, f) in raw["enemyFamilies"].as_object().unwrap() {
            for k in ["name", "plural", "group"] {
                if let Some(v) = f[k].as_str() {
                    known.insert(v.into());
                }
            }
        }
        for (_, v) in raw["items"].as_object().unwrap() {
            known.insert(v.as_str().unwrap().into());
        }
        for v in raw["npcs"].as_array().unwrap() {
            known.insert(v.as_str().unwrap().into());
        }

        let mut checked = 0;
        for j in board() {
            for field in ["questName", "questDescription"] {
                for e in elems(&j, field) {
                    if let Some(v) = e["localizationValue"].as_str() {
                        assert!(known.contains(v), "we emit {v}, which retail never sent");
                        checked += 1;
                    }
                }
            }
        }
        assert!(
            checked > 0,
            "no localization values emitted — the test proved nothing"
        );
    }
}

/// Our generated jobs board against the one committed retail board (tracker #92/#98).
///
/// The reporter says jobs still hang the map and asked me to compare our
/// `/quests` traffic with retail's. The API capture that would show live traffic
/// has been off since 28 August, so this diffs against
/// `blades-capture reference/capture-599.jsonl` instead — the one full retail
/// `/quests` body in the repo, 6 jobs.
#[cfg(test)]
mod jobs_wire_diff {
    use super::jobs_gen;
    use super::jobs_tests_support::sample_pools_for_diff as sample_pools;
    use std::collections::BTreeSet;
    use uuid::Uuid;

    /// Keys on a JOB entry, from the committed board (all 6 jobs carry exactly
    /// these 9).
    const RETAIL_JOB_KEYS: &[&str] = &[
        "completed",
        "difficultyLevel",
        "jobPoolId",
        "jobSetup",
        "objectiveStatuses",
        "questId",
        "seed",
        "type",
        "version",
    ];

    /// `jobSetup` keys present on ALL SIX retail jobs — the universal set.
    ///
    /// Derived from the capture, not assumed. My first version of this test took
    /// one Rescue job's 22 keys as universal and reported our Duel job as missing
    /// `primaryEnemyFamilyId`. Retail's own Duel job does not carry it either: it
    /// has 20 keys, no primary/secondary enemy family, and a `duelBossId` instead
    /// — which is exactly what we emit. The test was wrong, not the generator.
    const RETAIL_UNIVERSAL_JOBSETUP_KEYS: &[&str] = &[
        "algorithmVersion",
        "bossEnemyFamilyId",
        "bossLevelDelta",
        "dungeonTemplateId",
        "enemyBaseLevelOffset",
        "initialEPL",
        "jobCreatorVersion",
        "jobType",
        "primaryEnemyCount",
        "questDescription",
        "questName",
        "rewardGemCount",
        "rewardItemCount",
        "rewardItemId",
        "rewardXp",
        "secondaryEnemyCount",
        "secondaryEnemyCountPerSpawnerMax",
        "secondaryEnemyCountPerSpawnerMin",
        "secretRoom",
    ];

    /// The per-type and optional keys retail also used, on top of the universal
    /// set. Union across the six jobs; the two sets together are every key retail
    /// is known to send, so anything outside them is a key we invented.
    const RETAIL_OPTIONAL_JOBSETUP_KEYS: &[&str] = &[
        "duelBossId",
        "gatherItemCount",
        "gatherItemId",
        "primaryEnemyFamilyId",
        "rescueNpcCount",
        "secondaryEnemyFamilyId",
        "secretBossEnemyFamilyId",
        "secretBossLevelDelta",
    ];

    /// Keys we emit that the committed board CANNOT confirm, because it contains
    /// no job of the type that carries them.
    ///
    /// `defeatEnemyCount` is emitted on jobType 0 (Defeat), and the six retail
    /// jobs in the capture are types 3, 1, 5, 4, 4, 1 — no Defeat job at all. So
    /// this is neither confirmed nor refuted here, and allowing it silently would
    /// hide that. Settling it needs a Defeat job from the wider capture archive,
    /// which is on the box.
    const UNVERIFIED_BY_THE_SAMPLE: &[&str] = &["defeatEnemyCount"];

    #[test]
    fn our_job_entries_carry_every_key_retail_sent() {
        let pools = sample_pools();
        let char_id = Uuid::from_u128(7);
        let (jobs, _timers) = jobs_gen::generate(&pools, char_id, 30, 0, 0, 1_756_000_000);

        // Control: the generator must actually produce a board, or the assertions
        // below pass over an empty list and prove nothing.
        assert!(!jobs.is_empty(), "the generator produced no jobs at all");

        for job in &jobs {
            let obj = job.as_object().expect("a job must be an object");
            let ours: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
            for k in RETAIL_JOB_KEYS {
                assert!(
                    ours.contains(k),
                    "job entry is missing retail's `{k}`; we send {ours:?}"
                );
            }
            let setup = obj
                .get("jobSetup")
                .and_then(|v| v.as_object())
                .expect("every retail job carries a jobSetup");
            let sk: BTreeSet<&str> = setup.keys().map(String::as_str).collect();
            for k in RETAIL_UNIVERSAL_JOBSETUP_KEYS {
                assert!(
                    sk.contains(k),
                    "jobSetup is missing retail's universal `{k}` (jobType {:?}); we send {sk:?}",
                    setup.get("jobType")
                );
            }
            // The other direction, which matters just as much: a key retail never
            // sent is a shape it never produced, and inventing one is what the
            // stub-character load stall was.
            let known: BTreeSet<&str> = RETAIL_UNIVERSAL_JOBSETUP_KEYS
                .iter()
                .chain(RETAIL_OPTIONAL_JOBSETUP_KEYS.iter())
                .copied()
                .collect();
            let invented: Vec<&&str> = sk
                .iter()
                .filter(|k| !known.contains(**k) && !UNVERIFIED_BY_THE_SAMPLE.contains(*k))
                .collect();
            assert!(
                invented.is_empty(),
                "jobSetup carries keys retail never sent (jobType {:?}): {invented:?}",
                setup.get("jobType")
            );
        }
    }
}

#[cfg(test)]
mod completed_quests_key_tests {
    use super::*;
    use blades_lib::user_data::{Quest, QuestType};

    fn quest(kind: QuestType, gld: Uuid) -> Quest {
        Quest {
            version: 1,
            r#type: kind,
            objective_statuses: Default::default(),
            difficulty_level: 1,
            seed: serde_json::Number::from(1),
            gld_quest_id: gld,
            game_event_quest_data: None,
            rewards: None,
            final_reward: None,
            job_reward: None,
            completed: false,
        }
    }

    /// AN EVENT QUEST IS KEYED BY ITS OWN ID, NOT ITS TEMPLATE.
    ///
    /// The retail corpus is unambiguous: event `gldQuestId`s appear as a
    /// `completedQuests` key 0 times in 39, while event instance ids appear 110
    /// times in 1071. Keying by the template meant the tier counter was written
    /// where the client never looks, so rewards paid and nothing ever
    /// checkmarked (report #166).
    #[test]
    fn an_event_quest_is_keyed_by_its_instance_id() {
        let gld = Uuid::from_u128(0xAAAA);
        let instance = Uuid::from_u128(0xBBBB);
        assert_ne!(gld, instance, "the fixture must distinguish them");
        assert_eq!(
            completed_quests_key(&quest(QuestType::GameEvent, gld), instance),
            instance.to_string(),
        );
    }

    /// THE CONTROL, and it is why this bug survived: for an ordinary quest the
    /// two ids are equal in practice, so both readings agree (71/71 either way in
    /// the corpus). The rule must still pick the TEMPLATE there, so that a
    /// NORMAL quest whose ids happen to differ is unaffected by this change.
    #[test]
    fn a_normal_quest_is_still_keyed_by_its_template() {
        let gld = Uuid::from_u128(0xAAAA);
        let instance = Uuid::from_u128(0xBBBB);
        for kind in [QuestType::Normal, QuestType::Job] {
            assert_eq!(
                completed_quests_key(&quest(kind, gld), instance),
                gld.to_string(),
                "{kind:?} must keep the template key",
            );
        }
    }
}

/// A story quest must go out with retail's `difficultyLevel`, and jobs must not.
///
/// MEASURED over 773 captured `/quests` bodies: all 611 `quests[]` entries carry
/// `-1`, while `jobs[]` (4,444 entries) and `gameEventQuests[]` carry real enemy
/// levels from 1 to 84. So the sentinel is specific to story quests, and a test
/// that only checked "story quests are -1" would be satisfied by a repair that
/// flattened the other two arrays as well.
///
/// This existed because a retired code path minted story quests with a job-style
/// difficulty (and the tell-tale `seed: 1234`). Those rows are durable — 45 of
/// them across 34 characters — so fixing the accept path did nothing for players
/// already holding one, and the repair has to run on read.
#[cfg(test)]
mod story_quest_difficulty_repair {
    use super::*;
    use blades_lib::user_data::{ObjectiveStatus, Quest, QuestStatus, QuestType};
    use std::collections::HashMap;

    const STORY: Uuid = Uuid::from_u128(0xe0212e3f_5f6a_458c_8544_78d3532b2cb9);

    fn row(kind: QuestType, gld: Uuid, difficulty: i64) -> Quest {
        let mut objective_statuses = HashMap::new();
        objective_statuses.insert(
            Uuid::from_u128(1),
            ObjectiveStatus {
                status: QuestStatus::Active,
                progress: 0.0,
                completed: false,
            },
        );
        Quest {
            version: 2,
            r#type: kind,
            objective_statuses,
            difficulty_level: difficulty,
            seed: serde_json::Number::from(1234),
            gld_quest_id: gld,
            completed: false,
            game_event_quest_data: None,
            rewards: None,
            final_reward: None,
            job_reward: None,
        }
    }

    #[test]
    fn a_story_quest_stamped_with_a_job_difficulty_is_repaired() {
        // Flappety's row, from prod: the same quest another 19 characters hold
        // with -1. The only field that differed was this one.
        let mut q = row(QuestType::Normal, STORY, 3);
        assert!(
            repair_story_quest_difficulty(&mut q),
            "should report a change"
        );
        assert_eq!(q.difficulty_level, -1);
    }

    #[test]
    fn a_healthy_story_quest_is_left_alone_and_reports_no_write() {
        let mut q = row(QuestType::Normal, STORY, STORY_QUEST_DIFFICULTY_LEVEL);
        assert!(
            !repair_story_quest_difficulty(&mut q),
            "an already-correct row must not be rewritten on every /quests"
        );
        assert_eq!(q.difficulty_level, -1);
    }

    #[test]
    fn a_job_keeps_its_difficulty() {
        // Retail's jobs[] carry 1-84 here; flattening them to -1 would erase the
        // skull rating on the whole job board.
        let mut q = row(QuestType::Normal, jobs_gen::JOB_SENTINEL_GLD, 42);
        assert!(!repair_story_quest_difficulty(&mut q));
        assert_eq!(q.difficulty_level, 42, "a job's difficulty is a real level");
    }

    #[test]
    fn an_event_quest_keeps_its_difficulty() {
        let mut q = row(QuestType::GameEvent, STORY, 72);
        assert!(!repair_story_quest_difficulty(&mut q));
        assert_eq!(q.difficulty_level, 72);
    }

    /// The constant is the retail value, not merely "not what we had".
    #[test]
    fn the_sentinel_is_minus_one() {
        assert_eq!(STORY_QUEST_DIFFICULTY_LEVEL, -1);
    }
}

/// 2026-09-25: tapping QUESTS spun forever for every character tested.
///
/// The 05:00 UTC reset rolled boards whose Explore drew an Arena template (no kit
/// name) or whose Defeat drew a critter/duelist family (no enemy name), and both
/// came out with an empty `questName.dynamicElements`. Every `UI.Jobs.Names.*`
/// string has a placeholder ("Imperial Survey: {0}", "The {0} Menace"), and the
/// client's `Quest.BuildLocalizedQuestName` is a bare `String.Format`, so
/// `QuestMapMenuController.SetupQuestMap` threw `FormatException` from
/// `QuestMapMarkerJobDetailsProvider.SetupForID` on every refresh. Proven on the
/// emulator: with only those two names defused by a frida patch the map opens.
#[cfg(test)]
mod quest_map_wedge_2026_09_25 {
    use super::jobs_gen;
    use serde_json::Value;
    use uuid::Uuid;

    /// 2026-09-25 06:00 UTC, an hour into the board that wedged.
    const NOW: u64 = 1_790_312_400 + 3_600;
    const HALLOWEEN_TEST: &str = "7c2ed7c5-b63a-4ad7-b860-f80bfb885353";
    const FLAPPETY: &str = "30581f3e-75f5-41cf-a309-653f9802b56b";

    fn pools() -> Value {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        serde_json::from_str(&std::fs::read_to_string(dir.join("job_pools.json")).unwrap()).unwrap()
    }

    fn board(character: &str, now: u64) -> Vec<Value> {
        let pools = pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, now);
        jobs_gen::generate(
            &pools,
            Uuid::parse_str(character).unwrap(),
            86,
            0,
            boundary,
            now,
        )
        .0
    }

    fn job<'a>(jobs: &'a [Value], quest_id: &str) -> &'a Value {
        jobs.iter()
            .find(|j| j["questId"] == quest_id)
            .unwrap_or_else(|| panic!("{quest_id} is not on the board: this is not prod's board"))
    }

    fn name_elements(j: &Value) -> usize {
        j["jobSetup"]["questName"]["dynamicElements"]
            .as_array()
            .unwrap()
            .len()
    }

    /// The exact jobs production served with an empty name, now named.
    #[test]
    fn the_jobs_that_wedged_production_carry_their_names() {
        let ht = board(HALLOWEEN_TEST, NOW);
        let fl = board(FLAPPETY, NOW);
        for (jobs, id, key, want) in [
            (
                &ht,
                "e51dda6b-b7e6-49a0-a3bf-c64347e9f917",
                "UI.Jobs.Names.Explore.005",
                1,
            ),
            (
                &ht,
                "f7a75fca-402b-4d90-81f6-030f92903363",
                "UI.Jobs.Names.Defeat.005",
                3,
            ),
            (
                &fl,
                "060a79b5-c3e2-49f7-b56e-52c769a1f39e",
                "UI.Jobs.Names.Defeat.011",
                3,
            ),
        ] {
            let j = job(jobs, id);
            // Control: the same questId AND name key means we are regenerating
            // prod's board, where this job went out with `dynamicElements: []`.
            assert_eq!(j["jobSetup"]["questName"]["key"], key, "{id}");
            assert_eq!(
                name_elements(j),
                want,
                "{id} {key} must fill its placeholders"
            );
        }
    }

    /// The fix moves only the draws that could not be named. A job prod already
    /// served with names keeps its name key and dungeon.
    ///
    /// Its enemy families did move once since, on purpose: report #337 draws them
    /// from the families retail used (no Duelists, no critter leads, a variant at
    /// the job's level). This Goblin Wizard job became a Dremora Raider one, and
    /// its name elements follow the family, so it is still named.
    #[test]
    fn jobs_that_were_already_named_do_not_move() {
        let ht = board(HALLOWEEN_TEST, NOW);
        let goblins = &job(&ht, "c916c4f0-0323-45ac-ada5-c3b01bf74d7e")["jobSetup"];
        assert_eq!(
            goblins["primaryEnemyFamilyId"],
            "340cd608-31ec-447f-9f72-2162639bff3c"
        );
        assert_eq!(goblins["questName"]["key"], "UI.Jobs.Names.Defeat.010");
        // Report #306: this Defeat was rolled into `JobArenaVariant_04`, where no
        // non-duel job can be started. It moves to a kitted dungeon; its enemy and
        // name stay.
        assert_ne!(
            goblins["dungeonTemplateId"],
            "dbfd45fe-8c8c-4c8d-83c6-9b4566afc788"
        );
        assert!(!jobs_gen::DUEL_DUNGEON_TEMPLATES
            .contains(&goblins["dungeonTemplateId"].as_str().unwrap()));
        assert_eq!(
            goblins["questName"]["dynamicElements"][0]["localizationValue"],
            "Enemy.Name.DremoraRaider"
        );
        let lumber = &job(&ht, "87e29b6b-e4c7-4caa-afe2-60b24e9ea4aa")["jobSetup"];
        assert_eq!(
            lumber["dungeonTemplateId"],
            "57d639c2-ec4c-4e6b-9995-ff6a7ef3e712"
        );
        // Was the critter Spider (3d932102), which retail never made a primary.
        assert_eq!(
            lumber["primaryEnemyFamilyId"],
            "06591d48-8c3a-4f81-a2c6-dba2e7163788"
        );
        assert_eq!(lumber["questName"]["key"], "UI.Jobs.Names.Gather.008");
    }

    /// No board, for any character on any day, carries a job the client cannot
    /// name. 300 characters x 60 days is ~100k jobs; before the fix roughly one
    /// Defeat or Explore in three came out empty.
    #[test]
    fn no_board_ever_carries_an_unnameable_job() {
        let pools = pools();
        let (mut checked, mut defeat_or_explore) = (0usize, 0usize);
        for c in 1..=300u128 {
            let character = Uuid::from_u128(c * 0x9E37_79B9_7F4A_7C15);
            for day in 0..60u64 {
                let now = NOW + day * 86_400;
                let boundary = jobs_gen::current_reset_boundary(&pools, now);
                for j in jobs_gen::generate(&pools, character, 50, 0, boundary, now).0 {
                    let jt = j["jobSetup"]["jobType"].as_i64().unwrap();
                    if jt == 0 || jt == 1 {
                        defeat_or_explore += 1;
                    }
                    assert!(
                        name_elements(&j) > 0,
                        "{} on day {day} for {character}: {} has no name elements",
                        j["questId"],
                        j["jobSetup"]["questName"]["key"]
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 50_000, "only {checked} jobs checked");
        assert!(
            defeat_or_explore > checked / 4,
            "the sweep barely exercised Defeat/Explore"
        );
    }
}

/// Report #237: "Champion 1v1 fights in jobs should be in an arena, not a random map
/// location." A Duel drew its dungeon from the same pool as every other job, so most
/// of them were fought in a cave, forest, ruin or stone dungeon. Retail fought all 46
/// captured Duels in an arena (see `DUEL_DUNGEON_TEMPLATES`).
#[cfg(test)]
mod report_237_duel_arena {
    use super::jobs_gen;
    use serde_json::Value;
    use uuid::Uuid;

    /// 2026-09-26 16:52 UTC, the week the report was filed.
    const NOW: u64 = 1_790_441_545;
    const HALLOWEEN_TEST: &str = "7c2ed7c5-b63a-4ad7-b860-f80bfb885353";

    fn pools() -> Value {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        serde_json::from_str(&std::fs::read_to_string(dir.join("job_pools.json")).unwrap()).unwrap()
    }

    fn duels(pools: &Value, character: Uuid, level: u16, now: u64) -> Vec<Value> {
        let boundary = jobs_gen::current_reset_boundary(pools, now);
        jobs_gen::generate(pools, character, level, 0, boundary, now)
            .0
            .into_iter()
            .filter(|j| j["jobSetup"]["jobType"] == 5)
            .collect()
    }

    /// Every Duel, for any character in any week, is in one of the five arenas and
    /// is named after it.
    #[test]
    fn every_duel_is_fought_in_a_retail_arena() {
        let pools = pools();
        let mut seen = std::collections::BTreeMap::<String, u32>::new();
        let mut checked = 0;
        for c in 1..=300u128 {
            for week in 0..20u64 {
                let now = NOW + week * 7 * 86_400;
                for duel in duels(&pools, Uuid::from_u128(c * 0x9E37_79B9), 50, now) {
                    let js = &duel["jobSetup"];
                    let t = js["dungeonTemplateId"].as_str().unwrap();
                    assert!(
                        jobs_gen::DUEL_DUNGEON_TEMPLATES.contains(&t),
                        "Duel {} rolled outside the arenas: {t}",
                        duel["questId"]
                    );
                    let place = js["questName"]["dynamicElements"][1]["localizationValue"]
                        .as_str()
                        .unwrap_or_else(|| panic!("Duel {} has no location name", duel["questId"]));
                    assert!(place.starts_with("UI.Arena.Arena"), "{place}");
                    *seen.entry(t.to_string()).or_default() += 1;
                    checked += 1;
                }
            }
        }
        assert!(checked >= 6_000, "only {checked} Duels checked");
        // All five arenas are in play, as they were in retail (8-11 of 46 each).
        assert_eq!(
            seen.len(),
            jobs_gen::DUEL_DUNGEON_TEMPLATES.len(),
            "{seen:?}"
        );
    }

    /// The Duel production is serving HalloweenTest this week was rolled into
    /// `JobStoneVariant_03`. It moves into an arena, and nothing else about it moves:
    /// the dungeon pick is still one draw, so the champion and name stay what the
    /// player was already shown. The level/reward assertions below reflect the
    /// later report #279 difficulty correction: L86 jobs roll from retail's
    /// effective job baseline, not raw character level.
    #[test]
    fn a_duel_prod_served_in_a_stone_dungeon_moves_to_an_arena_and_nothing_else_moves() {
        let pools = pools();
        let ht = duels(&pools, Uuid::parse_str(HALLOWEEN_TEST).unwrap(), 86, NOW);
        let duel = ht
            .iter()
            .find(|j| j["questId"] == "18ae9573-ef4a-4ce3-bc24-715ea2d0d0b7")
            .expect("this is not prod's board");
        let js = &duel["jobSetup"];
        let t = js["dungeonTemplateId"].as_str().unwrap();
        assert_ne!(
            t, "4d3153a0-cfc5-405c-b065-92547ee9fbbc",
            "still in JobStoneVariant_03"
        );
        assert!(jobs_gen::DUEL_DUNGEON_TEMPLATES.contains(&t), "{t}");

        // Control: every value prod served before the fix is unchanged — except the
        // difficulty (prod: 76) and the XP that scales with it, which tracker #313
        // re-banded onto the retail difficulty cycle.
        assert_eq!(duel["difficultyLevel"], 70);
        assert_eq!(duel["seed"], -2_524_332_663_550_136_772_i64);
        assert_eq!(js["duelBossId"], "024b4f81-c7ef-4322-a547-ee863b4c02ad");
        assert_eq!(
            js["bossEnemyFamilyId"],
            "31be99a6-8557-4e9b-81e6-5503f900b7d2"
        );
        assert_eq!(js["bossLevelDelta"], 7);
        assert_eq!(js["rewardXp"], 1476); // retail's for difficulty 70 (#337); prod: 1543 at 76
        assert_eq!(js["rewardItemCount"], 0);
        assert_eq!(js["questName"]["key"], "UI.Jobs.Names.Duel.002");
        assert_eq!(
            js["questName"]["dynamicElements"][0]["localizationValue"],
            "NPC.Duelist3.Name"
        );
    }

    /// Control: only a Duel is narrowed. Defeat, Explore, Clear, Rescue and Gather
    /// draw from exactly the pool they always did.
    #[test]
    fn other_job_types_keep_their_pool() {
        for t in [0, 1, 2, 3, 4] {
            assert_eq!(
                jobs_gen::dungeon_pool(t),
                jobs_gen::dungeon_pool(-1),
                "jobType {t}"
            );
            assert!(jobs_gen::dungeon_pool(t).len() > jobs_gen::DUEL_DUNGEON_TEMPLATES.len());
        }
    }
}

/// Report #279 follow-up: L100 town jobs were lethal because we rolled job
/// difficulty from raw character level. Retail L100 boards in the 2026-06-07
/// snapshot carry `jobSetup.initialEPL` 83-84 and `difficultyLevel` 74-84; our
/// L100 rows carried 87-97 and generated enemies at those same levels.
#[cfg(test)]
mod report_279_job_difficulty {
    use super::*;
    use serde_json::Value;
    use uuid::Uuid;

    const NOW: u64 = 1_790_618_400;

    fn pools() -> Value {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        serde_json::from_str(&std::fs::read_to_string(dir.join("job_pools.json")).unwrap()).unwrap()
    }

    fn l100_board(character: Uuid) -> Vec<Value> {
        l100_board_at(character, 0)
    }

    /// Characters sit at every point of the 80-entry difficulty cycle, and only a
    /// few points name the very-hard band, so the sweep below walks them all.
    fn l100_board_at(character: Uuid, cycle_index: i64) -> Vec<Value> {
        let pools = pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW);
        jobs_gen::generate(&pools, character, 100, cycle_index, boundary, NOW).0
    }

    #[test]
    fn level_100_jobs_roll_from_retail_effective_level_not_raw_player_level() {
        let mut checked = 0;
        let mut saw_top_retail_level = false;
        for c in 1..=200u128 {
            let cycle_index = (c % 80) as i64;
            for job in l100_board_at(Uuid::from_u128(c * 0x9E37_79B9_7F4A_7C15), cycle_index) {
                let setup = &job["jobSetup"];
                assert_eq!(
                    setup["initialEPL"], 84,
                    "retail L100 jobs use an 83-84 effective baseline, not raw level 100"
                );
                let difficulty = job["difficultyLevel"].as_i64().unwrap();
                assert!(
                    (73..=84).contains(&difficulty),
                    "L100 job {} rolled difficulty {difficulty}; retail sample is 74-84",
                    job["questId"]
                );
                saw_top_retail_level |= difficulty == 84;
                checked += 1;
            }
        }
        assert!(checked > 1_000, "only {checked} jobs checked");
        assert!(
            saw_top_retail_level,
            "the sweep never exercised the top retail level"
        );
    }

    #[test]
    fn generated_enemies_follow_the_softened_job_difficulty() {
        let gd = super::report85_job_generated_data_tests::game_data();
        for job in l100_board(Uuid::from_u128(0x279)) {
            let difficulty = job["difficultyLevel"].as_i64().unwrap();
            let boss_delta = job["jobSetup"]["bossLevelDelta"].as_i64().unwrap_or(0);
            let data =
                jobs_gen::generated_data_for_job(&gd, &job, &crate::quest::shipped_scaling())
                    .expect("job generates");
            let mut levels = Vec::new();
            for enemy in data.enemy_generated_data.values().flatten().flatten() {
                levels.push(enemy.enemy_level);
            }
            assert!(!levels.is_empty(), "job has generated enemies");
            assert_eq!(levels.iter().copied().min(), Some(difficulty));
            assert!(
                levels.iter().copied().max().unwrap() <= difficulty + boss_delta + 2,
                "generated levels {levels:?} escaped the job's own softened difficulty"
            );
            assert!(
                levels.iter().all(|level| *level <= 92),
                "raw-L100 scaling leaked back into generated data: {levels:?}"
            );
        }
    }

    #[test]
    fn control_an_explicit_job_row_keeps_its_declared_difficulty() {
        let gd = super::report85_job_generated_data_tests::game_data();
        let job = json!({
            "questId": "00000000-0000-4000-8000-000000000279",
            "version": 0,
            "type": "JOB",
            "objectiveStatuses": {},
            "difficultyLevel": 97,
            "seed": 0,
            "jobSetup": {
                "jobType": 0,
                "bossLevelDelta": 5,
                "rewardXp": 0,
                "rewardItemCount": 0
            },
            "completed": false
        });
        let entry = jobs_gen::job_quest_db_entry(
            &job,
            Uuid::from_u128(0x279),
            &gd,
            &crate::quest::shipped_scaling(),
        )
        .expect("row builds");
        assert_eq!(entry.info.0.difficulty_level, 97);
        let generated = entry
            .generated_data
            .0
            .expect("job rows carry generated data");
        let min_level = generated
            .enemy_generated_data
            .values()
            .flatten()
            .flatten()
            .map(|enemy| enemy.enemy_level)
            .min();
        assert_eq!(
            min_level,
            Some(97),
            "stored job rows preserve the declared level"
        );
    }
}

/// Tracker #313 (Raysiel, new character): every job on the board was five-skull
/// "very hard", the recommended level sat above his own, and it rose as he levelled.
///
/// The baseline was right (`initialEPL`, #279) but the offset on top of it was not.
/// We drew it uniformly over the whole `veryEasyMin..=veryHardMax` span, -2..+9 below
/// level 20, so half of every board was hard or very hard. Retail draws it from ONE
/// band of that span, the band `globals.difficultyCycle` (80 entries, already in
/// `job_pools.json`) names at the character's `jobDifficultyCycleIndex` plus the job's
/// place on the board.
///
/// MEASURED in the 2026-06-07 snapshot, 486 distinct jobs from 13 characters:
///
/// ```text
/// 157 jobs at levels 1-20, difficultyLevel - initialEPL:
///   -4:2 -3:2 -2:18 -1:27 | 0:38 1:30 | 2:11 3:15 | 4:3 5:9 | 6:1 8:1
///   very easy 49 (31%) | easy 68 (43%) | normal 26 (17%) | hard 12 (8%) | very hard 2 (1%)
///   mean +0.58 (the uniform roll: +3.5)
/// difficultyCycle band weights: 25% / 50% / 15% / 7.5% / 2.5%
/// 37 of 42 consecutive board refreshes carry exactly the bands
///   difficultyCycle[prevIndex .. newIndex] predicts (against initialEPL; 26/42 against
///   character level); the misses are level-1 boards, where the floor of 1 folds
///   "very easy" into "easy".
/// ```
#[cfg(test)]
mod report_313_job_difficulty_bands {
    use super::*;
    use serde_json::Value;
    use uuid::Uuid;

    const NOW: u64 = 1_790_618_400;
    const CYCLE_LEN: i64 = 80;

    fn pools() -> Value {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        serde_json::from_str(&std::fs::read_to_string(dir.join("job_pools.json")).unwrap()).unwrap()
    }

    /// `(difficultyLevel - initialEPL)` for every job on boards rolled at `levels`,
    /// over many characters and every point of the cycle.
    fn offsets(levels: std::ops::RangeInclusive<u16>) -> Vec<i64> {
        let pools = pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW);
        let mut out = Vec::new();
        for level in levels {
            for c in 0..CYCLE_LEN as u128 {
                let character = Uuid::from_u128((c + 1) * 0x9E37_79B9_7F4A_7C15 + level as u128);
                let (jobs, _) =
                    jobs_gen::generate(&pools, character, level, c as i64, boundary, NOW);
                for job in jobs {
                    let epl = job["jobSetup"]["initialEPL"].as_i64().unwrap();
                    out.push(job["difficultyLevel"].as_i64().unwrap() - epl);
                }
            }
        }
        out
    }

    /// Levels 3-17: `initialEPL` 4-19, so the level-1 row applies (bands -2/0/2/4/6..9)
    /// and the floor of 1 never folds a very-easy roll.
    #[test]
    fn low_level_boards_follow_the_retail_band_mix() {
        let offs = offsets(3..=17);
        assert!(offs.len() > 3_000, "only {} jobs", offs.len());
        let n = offs.len() as f64;
        let share = |f: &dyn Fn(i64) -> bool| offs.iter().filter(|o| f(**o)).count() as f64 / n;
        let mean = offs.iter().sum::<i64>() as f64 / n;

        let easy_or_below = share(&|o| o < 2);
        let hard_or_above = share(&|o| o >= 4);
        // Retail 117/157 = 75% easy or very easy; the uniform roll gave 4/12 = 33%.
        assert!(
            (0.65..=0.85).contains(&easy_or_below),
            "{:.0}% of jobs easy or very easy; retail 75%",
            easy_or_below * 100.0
        );
        // Retail 14/157 = 9% hard or very hard; the uniform roll gave 6/12 = 50%.
        assert!(
            hard_or_above <= 0.15,
            "{:.0}% of jobs hard or very hard; retail 9%",
            hard_or_above * 100.0
        );
        // Retail mean +0.58 over initialEPL; the uniform roll's was +3.5.
        assert!((0.0..=1.5).contains(&mean), "mean offset {mean:.2}; retail +0.58");
        // The whole retail span is still reachable, the very-hard top included.
        assert_eq!(offs.iter().min(), Some(&-2));
        assert_eq!(offs.iter().max(), Some(&9));
    }

    /// Each job lands in the band the cycle names for its place on the board.
    #[test]
    fn each_job_takes_the_band_the_cycle_names() {
        let pools = pools();
        let cycle: Vec<i64> = pools["globals"]["difficultyCycle"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(cycle.len() as i64, CYCLE_LEN);
        // Level-1 row: very easy -2..-1, easy 0..1, normal 2..3, hard 4..5, very hard 6..9.
        let band_of = |o: i64| match o {
            ..=-1 => 0,
            0..=1 => 1,
            2..=3 => 2,
            4..=5 => 3,
            _ => 4,
        };
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW);
        for start in 0..CYCLE_LEN {
            let character = Uuid::from_u128(0x313_0000 + start as u128);
            let (jobs, _) = jobs_gen::generate(&pools, character, 9, start, boundary, NOW);
            assert!(jobs.len() >= 4);
            for (k, job) in jobs.iter().enumerate() {
                let epl = job["jobSetup"]["initialEPL"].as_i64().unwrap();
                let offset = job["difficultyLevel"].as_i64().unwrap() - epl;
                let want = cycle[((start + k as i64) % CYCLE_LEN) as usize];
                assert_eq!(
                    band_of(offset),
                    want,
                    "cycle index {start}, board slot {k}: offset {offset}"
                );
            }
        }
    }

    /// Raysiel's level, at the start of the cycle (a fresh character): retail's first
    /// board on three fresh characters was very easy, easy, normal, easy, very easy,
    /// hard. Ours, against `initialEPL` 10, must not hold a single very-hard job.
    #[test]
    fn a_fresh_level_9_board_is_not_wall_to_wall_very_hard() {
        let pools = pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW);
        let character = Uuid::parse_str("84aeca4f-e7a8-479f-afb5-ec405f3bcce1").unwrap();
        let (jobs, _) = jobs_gen::generate(&pools, character, 9, 0, boundary, NOW);
        let offs: Vec<i64> = jobs
            .iter()
            .map(|j| {
                j["difficultyLevel"].as_i64().unwrap()
                    - j["jobSetup"]["initialEPL"].as_i64().unwrap()
            })
            .collect();
        assert!(offs.iter().all(|o| *o <= 5), "board offsets {offs:?}");
        assert!(offs.iter().filter(|o| **o <= 1).count() >= 3, "board offsets {offs:?}");
    }

    /// Replenishing after a completion keeps every surviving job exactly as it was.
    #[test]
    fn control_a_replenished_board_keeps_the_surviving_jobs_unchanged() {
        let pools = pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, NOW);
        let character = Uuid::from_u128(0x313);
        let (base, _) = jobs_gen::generate(&pools, character, 9, 17, boundary, NOW);
        let done: std::collections::HashSet<Uuid> =
            [Uuid::parse_str(base[0]["questId"].as_str().unwrap()).unwrap()].into();
        let (after, _) =
            jobs_gen::generate_replenished(&pools, character, 9, 17, boundary, NOW, &done);
        for job in &base[1..] {
            let same = after.iter().find(|j| j["questId"] == job["questId"]).expect("kept");
            assert_eq!(same["difficultyLevel"], job["difficultyLevel"]);
        }
    }
}

/// Tracker #306 (HauDrauf, 2026-10-02 05:06 UTC): the highlighted job
/// "Unendlich viel: Holz" (`UI.Jobs.Names.Gather.007` + Lumber, 13 Gems) hung on
/// tap and no `…/dungeons/current/enter` was ever sent.
///
/// Rebuilt from his character id and the reset window, that job was
/// `36df4240-…`: slot 1 of the Thursday featured pool, i.e. a REPLACEMENT minted
/// after he completed the featured job `33ec5677-…` on 2026-10-01 05:28, and a
/// Gather rolled into `JobArenaVariant_06`. Retail does neither: a featured or
/// boss pool (`maxTotal` 1) is not refilled once completed, and no non-duel job is
/// ever in an arena. Its disappearance after relaunch was the 05:00 daily reset.
#[cfg(test)]
mod report_306_daily_job_hang {
    use super::jobs_gen;
    use serde_json::Value;
    use std::collections::HashSet;
    use uuid::Uuid;

    const HAUDRAUF: &str = "489620db-7f90-4a03-bb7c-f7e92a9c73cb";
    /// 2026-10-02 03:01:31 UTC — his last `/quests` before the hang (Oct 1 window).
    const OCT1_FETCH: u64 = 1_790_910_091;
    /// 2026-10-02 05:09:56 UTC — the relaunch `/quests` (Oct 2 window).
    const OCT2_FETCH: u64 = 1_790_917_796;
    const STANDARD_POOL: &str = "4956c6ab-1832-4edd-8bee-561b79f83ee2";
    const BOSS_POOL: &str = "361da91e-6860-4c31-a447-4010cbaad1dd";
    /// The weekly "featured" pool active during the Thursday game-day.
    const THURSDAY_FEATURED_POOL: &str = "df666a07-3539-426a-916e-ccdba580cb1d";

    fn pools() -> Value {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        serde_json::from_str(&std::fs::read_to_string(dir.join("job_pools.json")).unwrap()).unwrap()
    }
    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }
    fn ids(jobs: &[Value]) -> HashSet<Uuid> {
        jobs.iter().map(|j| uuid(j["questId"].as_str().unwrap())).collect()
    }
    fn board(now: u64, completed: &[&str]) -> Vec<Value> {
        let pools = pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, now);
        let completed: HashSet<Uuid> = completed.iter().map(|s| uuid(s)).collect();
        jobs_gen::generate_replenished(&pools, uuid(HAUDRAUF), 100, 0, boundary, now, &completed).0
    }
    fn job<'a>(jobs: &'a [Value], id: &str) -> &'a Value {
        jobs.iter()
            .find(|j| j["questId"] == id)
            .unwrap_or_else(|| panic!("{id} is not on this board"))
    }
    fn is_arena(job: &Value) -> bool {
        jobs_gen::DUEL_DUNGEON_TEMPLATES
            .contains(&job["jobSetup"]["dungeonTemplateId"].as_str().unwrap())
    }

    /// The jobs he completed in the Oct 1 window (prod journal, 05:23 and 05:28).
    const DONE_BOSS: &str = "313ada72-cf93-4559-bda8-99719155e672";
    const DONE_FEATURED: &str = "33ec5677-352a-4205-9290-4fd3d1770127";
    /// The replacements the old code minted; the second is "Unendlich viel: Holz".
    const REFILLED_BOSS: &str = "3091050e-c10b-4905-a1d1-4455fcec3b05";
    const UNENDLICH_VIEL_HOLZ: &str = "36df4240-b3c3-4d6b-be62-57e83b00fb4e";

    #[test]
    fn a_completed_featured_or_boss_job_is_not_replaced() {
        let untouched = board(OCT1_FETCH, &[]);
        // Control: these really are his Oct 1 board's boss and featured jobs.
        assert_eq!(job(&untouched, DONE_BOSS)["jobPoolId"], BOSS_POOL);
        assert_eq!(job(&untouched, DONE_FEATURED)["jobPoolId"], THURSDAY_FEATURED_POOL);
        assert_eq!(untouched.len(), 6, "4 standard + boss + featured");

        let after = board(OCT1_FETCH, &[DONE_BOSS, DONE_FEATURED]);
        let after_ids = ids(&after);
        for gone in [DONE_BOSS, DONE_FEATURED, REFILLED_BOSS, UNENDLICH_VIEL_HOLZ] {
            assert!(!after_ids.contains(&uuid(gone)), "{gone} is on the board");
        }
        assert!(
            after.iter().all(|j| j["jobPoolId"] == STANDARD_POOL),
            "retail leaves a completed maxTotal-1 pool empty for the window"
        );
        assert_eq!(after.len(), 4, "the board shrinks by the two, as retail's did");
        let standard: HashSet<Uuid> = untouched
            .iter()
            .filter(|j| j["jobPoolId"] == STANDARD_POOL)
            .map(|j| uuid(j["questId"].as_str().unwrap()))
            .collect();
        assert_eq!(after_ids, standard, "the four standard jobs are untouched");
    }

    /// Negative control: the standard pool (`maxTotal` 100) still refills, which is
    /// what retail did for all 19 captured standard-pool completions.
    #[test]
    fn a_completed_standard_job_is_still_replaced() {
        let untouched = board(OCT1_FETCH, &[]);
        let done = untouched
            .iter()
            .find(|j| j["jobPoolId"] == STANDARD_POOL)
            .unwrap()["questId"]
            .as_str()
            .unwrap()
            .to_string();
        let after = board(OCT1_FETCH, &[&done]);
        assert_eq!(after.len(), untouched.len(), "the board stays full");
        assert!(!ids(&after).contains(&uuid(&done)));
        let new: Vec<&Value> = after
            .iter()
            .filter(|j| !ids(&untouched).contains(&uuid(j["questId"].as_str().unwrap())))
            .collect();
        assert_eq!(new.len(), 1, "exactly one replacement");
        assert_eq!(new[0]["jobPoolId"], STANDARD_POOL);
    }

    /// His board after the relaunch (these six ids are the rows prod stored at
    /// 05:09:56) carried a Gather in `JobArenaVariant_05` and a Rescue in
    /// `JobArenaVariant_01` — the same shape as the job that hung. They move to a
    /// kitted dungeon, and nothing else about them, or about the rest, moves.
    #[test]
    fn non_duel_jobs_leave_the_arenas_and_nothing_else_moves() {
        let jobs = board(OCT2_FETCH, &[]);
        assert_eq!(
            ids(&jobs),
            [
                "a4a54f84-390f-4031-bf3a-c8fc9e8e11ed",
                "72217ae0-4bdb-4079-973b-4ca252ee487f",
                "f5a09068-c422-4fe8-a5ab-e028233e1dcd",
                "1220dc37-9166-4364-be77-a3ff32fe40aa",
                "8e86eb3a-4398-4ef3-9a8d-7e63e27605f7",
                "de78d8f4-bff9-4942-a6d0-dd288b44535d",
            ]
            .iter()
            .map(|s| uuid(s))
            .collect::<HashSet<_>>(),
            "not the board prod stored for him"
        );

        // (id, type, arena it was in, values prod served that must not move)
        for (id, ty, old_arena, gems, xp, name_key) in [
            ("a4a54f84-390f-4031-bf3a-c8fc9e8e11ed", 4, "19a3b1b0-c18b-4f2f-b73f-780f3759fe48", 0, 1516, "UI.Jobs.Names.Gather.004"),
            // XP 1526: retail's for difficulty 75 (#337). Prod served 1593 before
            // tracker #313 re-banded the difficulty; the draw itself is unchanged.
            // Gems 0: prod served a 10-gem secret-room roll on this standard job;
            // retail pays standard jobs no gems (#306, `report_306_job_gems`).
            ("72217ae0-4bdb-4079-973b-4ca252ee487f", 3, "e7418cc7-01de-4c84-ba00-e221f8783d51", 0, 1526, "UI.Jobs.Names.Rescue.003"),
        ] {
            let j = job(&jobs, id);
            let js = &j["jobSetup"];
            assert_eq!(js["jobType"], ty);
            assert_ne!(js["dungeonTemplateId"], old_arena, "{id} still in its arena");
            assert!(!is_arena(j), "{id} moved into another arena");
            let names = js["questName"]["dynamicElements"].as_array().unwrap();
            let place = names.last().unwrap()["localizationValue"].as_str().unwrap();
            assert!(place.starts_with("UI.Jobs.Location.Name."), "{id}: {place}");
            assert_eq!(js["rewardGemCount"], gems);
            assert_eq!(js["rewardXp"], xp);
            assert_eq!(js["questName"]["key"], name_key);
        }
        let gather = &job(&jobs, "a4a54f84-390f-4031-bf3a-c8fc9e8e11ed")["jobSetup"];
        assert_eq!(gather["gatherItemId"], "da767378-8c00-43c1-a5eb-705d7d2f7306");
        assert_eq!(gather["gatherItemCount"], 6);

        // Control: the jobs that were not in an arena keep the dungeon prod served.
        for (id, dungeon) in [
            ("f5a09068-c422-4fe8-a5ab-e028233e1dcd", "18e81559-3561-47ef-b73e-9f3bc34ba0b8"),
            ("1220dc37-9166-4364-be77-a3ff32fe40aa", "86e6a720-caa4-4b13-b8b8-73f3dcf049a2"),
            ("de78d8f4-bff9-4942-a6d0-dd288b44535d", "57d639c2-ec4c-4e6b-9995-ff6a7ef3e712"),
            // The Duel stays in its arena.
            ("8e86eb3a-4398-4ef3-9a8d-7e63e27605f7", "a9386df1-5b26-462b-9c56-de9cb371c790"),
        ] {
            assert_eq!(job(&jobs, id)["jobSetup"]["dungeonTemplateId"], dungeon, "{id}");
        }
    }

    /// Retail, 2026-06-07 snapshot: 0 of 417 non-duel jobs in an arena, and each
    /// of Defeat, Explore, Rescue and Gather used all 12 kitted templates.
    #[test]
    fn no_non_duel_job_is_ever_in_an_arena() {
        let pools = pools();
        let mut per_type = std::collections::BTreeMap::<i64, HashSet<String>>::new();
        let mut checked = 0;
        for c in 1..=200u128 {
            for day in 0..28u64 {
                let now = OCT2_FETCH + day * 86_400;
                let boundary = jobs_gen::current_reset_boundary(&pools, now);
                let (jobs, _) =
                    jobs_gen::generate(&pools, Uuid::from_u128(c * 0x9E37_79B9), 60, 0, boundary, now);
                for j in jobs {
                    let js = &j["jobSetup"];
                    let ty = js["jobType"].as_i64().unwrap();
                    if ty == 5 {
                        assert!(is_arena(&j), "a Duel left the arenas: {}", j["questId"]);
                        continue;
                    }
                    assert!(!is_arena(&j), "jobType {ty} in an arena: {}", j["questId"]);
                    for e in js["questName"]["dynamicElements"].as_array().unwrap() {
                        let v = e["localizationValue"].as_str().unwrap_or("");
                        assert!(!v.starts_with("UI.Arena."), "{} named {v}", j["questId"]);
                    }
                    per_type
                        .entry(ty)
                        .or_default()
                        .insert(js["dungeonTemplateId"].as_str().unwrap().to_string());
                    checked += 1;
                }
            }
        }
        assert!(checked > 15_000, "only {checked} non-duel jobs checked");
        for (ty, used) in &per_type {
            assert_eq!(used.len(), 12, "jobType {ty} used {used:?}");
        }
    }
}

/// Tracker #306 follow-up (HauDrauf, 2026-10-03): "the two new daily jobs are here,
/// but no gem reward is set." His Oct 3 board's highlighted jobs, the boss Duel
/// `4b25489e-…` and the Friday featured Rescue `71b53f82-…`, both showed 0 gems
/// and paid gold.
///
/// MINED from the retail snapshot (222 `/quests` bodies, 13 characters, 463
/// distinct jobs, levels 1-100):
///
/// ```text
/// pool                      jobs  rewardGemCount   rewardItemCount (gold)
/// standard (presentation 0)  352  0 on 352         > 0 on 352
/// featured weekly (1)         84  4 on 84          0 on 84
/// boss weekly (2)             27  12 on 27         0 on 27
/// featured daily (1, daily)    0  (dormant)
/// ```
///
/// Flat in every level band, and not tied to the secret room: 0 of 109 standard
/// secret-room jobs paid gems, while 44 of the 84 featured gem jobs had no secret
/// room. Every job /complete for a gem job paid `currencies: {gems: N}` and no gold
/// (captures 9303, 11388, 18482: 4; 11253, 37130: 12).
///
/// Our roll gave gems only on a third of secret-room jobs (6-15, any pool) and never
/// on a boss Duel (no secret room), and `job_completion_reward` never paid gems.
#[cfg(test)]
mod report_306_job_gems {
    use super::jobs_gen;
    use blades_lib::economy::GEMS;
    use serde_json::{json, Value};
    use uuid::Uuid;

    const HAUDRAUF: &str = "489620db-7f90-4a03-bb7c-f7e92a9c73cb";
    /// 2026-10-03 05:06 UTC, inside the window his prod board was rolled for
    /// (`lastJobsResetTime` 1791003600, `jobDifficultyCycleIndex` 60).
    const OCT3_FETCH: u64 = 1_791_004_000;
    const GOLD: Uuid = Uuid::from_u128(0xf8d27767_a85e_4fd6_a5bb_bf8a13d0daa2);

    fn pools() -> Value {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        serde_json::from_str(&std::fs::read_to_string(dir.join("job_pools.json")).unwrap()).unwrap()
    }

    fn presentation_of(pools: &Value, pool_id: &str) -> i64 {
        pools["jobPools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["jobPoolId"] == pool_id)
            .and_then(|p| p["presentation"].as_i64())
            .unwrap_or_else(|| panic!("unknown pool {pool_id}"))
    }

    /// Every job across many characters, levels and every weekday, tagged with
    /// its pool's presentation.
    fn sample() -> Vec<(i64, Value)> {
        let pools = pools();
        let mut out = Vec::new();
        for day in 0..7u64 {
            let now = OCT3_FETCH + day * 86_400;
            let boundary = jobs_gen::current_reset_boundary(&pools, now);
            for seed in 0..40u128 {
                let c = Uuid::from_u128(0x306_0000 + seed);
                for level in [1u16, 20, 45, 70, 100] {
                    for j in jobs_gen::generate(&pools, c, level, 0, boundary, now).0 {
                        let p = presentation_of(&pools, j["jobPoolId"].as_str().unwrap());
                        out.push((p, j));
                    }
                }
            }
        }
        out
    }

    fn gems(j: &Value) -> u64 {
        j["jobSetup"]["rewardGemCount"].as_u64().unwrap()
    }
    fn gold(j: &Value) -> u64 {
        j["jobSetup"]["rewardItemCount"].as_u64().unwrap()
    }

    #[test]
    fn a_featured_job_pays_four_gems_and_no_gold() {
        let featured: Vec<_> = sample().into_iter().filter(|(p, _)| *p == 1).collect();
        assert!(featured.len() >= 7 * 40, "every weekday rolls a featured job");
        for (_, j) in &featured {
            assert_eq!(gems(j), 4, "retail: 84 of 84 featured jobs paid 4 gems");
            assert_eq!(gold(j), 0, "retail: 84 of 84 featured jobs paid no gold");
        }
    }

    #[test]
    fn a_boss_job_pays_twelve_gems_and_no_gold() {
        let boss: Vec<_> = sample().into_iter().filter(|(p, _)| *p == 2).collect();
        assert!(!boss.is_empty());
        for (_, j) in &boss {
            assert_eq!(gems(j), 12, "retail: 27 of 27 boss jobs paid 12 gems");
            assert_eq!(gold(j), 0, "retail: 27 of 27 boss jobs paid no gold");
        }
    }

    /// The control, and the secret-room half of the old rule.
    #[test]
    fn a_standard_job_pays_gold_and_never_gems_secret_room_or_not() {
        let standard: Vec<_> = sample().into_iter().filter(|(p, _)| *p == 0).collect();
        let secret = standard.iter().filter(|(_, j)| j["jobSetup"]["secretRoom"] == true).count();
        assert!(secret > 100, "the sample holds secret-room standard jobs ({secret})");
        for (_, j) in &standard {
            assert_eq!(gems(j), 0, "retail: 0 of 352 standard jobs paid gems");
            assert!(gold(j) > 0, "retail: 352 of 352 standard jobs paid gold");
        }
    }

    /// Identity test against retail's own /complete: capture 9303 (featured, 4
    /// gems, 94 xp) and 11253 (boss, 12 gems, 148 xp) paid exactly these.
    #[test]
    fn completing_a_gem_job_pays_the_gems() {
        for (gems, xp) in [(4u64, 94u64), (12, 148)] {
            let job = json!({
                "questId": "00000000-0000-0000-0000-000000000306",
                "jobSetup": {
                    "rewardGemCount": gems,
                    "rewardItemId": GOLD.to_string(),
                    "rewardItemCount": 0,
                    "rewardXp": xp,
                },
            });
            let r = jobs_gen::job_completion_reward(&job);
            assert_eq!(r.currencies.len(), 1, "{:?}", r.currencies);
            assert_eq!(r.currencies.get(&GEMS), Some(&gems));
            assert_eq!(r.character_xp, xp);
        }
        // Control: a standard job still pays its gold and no gems.
        let r = jobs_gen::job_completion_reward(&json!({
            "jobSetup": {"rewardGemCount": 0, "rewardItemId": GOLD.to_string(),
                         "rewardItemCount": 520, "rewardXp": 310},
        }));
        assert_eq!(r.currencies.get(&GOLD), Some(&520));
        assert!(!r.currencies.contains_key(&GEMS));
    }

    /// His actual Oct 3 board (these six ids are the rows prod stored at the 05:00
    /// reset). The highlighted two now pay gems; every other value — ids, names,
    /// dungeons, difficulty, the standard jobs' gold — is what prod served, so the
    /// fix moves no other draw. (XP is retail's table for the difficulty since #337.)
    #[test]
    fn his_oct3_board_pays_gems_on_the_highlighted_jobs_and_nothing_else_moves() {
        let pools = pools();
        let boundary = jobs_gen::current_reset_boundary(&pools, OCT3_FETCH);
        assert_eq!(boundary, 1_791_003_600, "his stored lastJobsResetTime");
        let c = Uuid::parse_str(HAUDRAUF).unwrap();
        let jobs = jobs_gen::generate(&pools, c, 100, 60, boundary, OCT3_FETCH).0;
        // (id, pool prefix, difficulty, xp, name, gems, gold)
        let want = [
            ("a398cf65-6793-4a69-928e-f3492bb78ea9", "4956c6ab", 76, 1536, "UI.Jobs.Names.Defeat.009", 0, 1640),
            ("346135d3-ad76-4658-b0f2-af7fb77c4a0d", "4956c6ab", 79, 1566, "UI.Jobs.Names.Rescue.006", 0, 1650),
            ("01cda05c-b403-4ef6-97ed-a987f47b4a69", "4956c6ab", 75, 1526, "UI.Jobs.Names.Rescue.004", 0, 1650),
            ("de1a54e3-7889-4e9b-a6d3-e24f344fdb5d", "4956c6ab", 73, 1506, "UI.Jobs.Names.Rescue.001", 0, 1570),
            // boss Duel: prod served 0 gems / 1620 gold
            ("4b25489e-7f63-4bac-8680-69f150deaa40", "361da91e", 75, 1526, "UI.Jobs.Names.Duel.002", 12, 0),
            // Friday featured Rescue: prod served 0 gems / 1660 gold
            ("71b53f82-cab9-4580-8012-ffba1c12d9c5", "8501a030", 81, 1586, "UI.Jobs.Names.Rescue.001", 4, 0),
        ];
        assert_eq!(jobs.len(), want.len());
        for (id, pool, d, xp, name, gem, gold_) in want {
            let j = jobs.iter().find(|j| j["questId"] == id).unwrap_or_else(|| panic!("{id} missing"));
            let s = &j["jobSetup"];
            assert!(j["jobPoolId"].as_str().unwrap().starts_with(pool), "{id}");
            assert_eq!(j["difficultyLevel"], d, "{id}");
            assert_eq!(s["rewardXp"], xp, "{id}");
            assert_eq!(s["questName"]["key"], name, "{id}");
            assert_eq!(s["rewardGemCount"], gem, "{id}");
            assert_eq!(s["rewardItemCount"], gold_, "{id}");
        }
        assert_eq!(
            jobs.iter().find(|j| j["questId"] == "4b25489e-7f63-4bac-8680-69f150deaa40").unwrap()
                ["jobSetup"]["duelBossId"],
            "01d82726-527f-4601-929c-182acd3fa9b7"
        );
    }
}

#[cfg(test)]
mod report323_event_row_stage_repair_tests {
    use super::*;
    use blades_lib::static_data::StaticData;

    /// EQ24, open on 2026-10-03; stage `_A` is the one its template names.
    const EQ24: &str = "816ff4c8-b56f-4645-bd2a-29bc7c1baf96";
    const EQ24_A: &str = "88d3d9f4-fb63-4d2a-af60-66ef1ba74736";
    const EQ24_B: &str = "6988a711-96b0-46d4-9264-c1c06216d621";
    const CHAR: Uuid = Uuid::from_u128(0xf781_7aa6);
    const REPORT_323_NOW: i64 = 1_791_007_200;

    fn static_data() -> StaticData {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        crate::static_loader::load(&dir)
    }

    fn game_data() -> blades_lib::game_data::GameData {
        super::report85_job_generated_data_tests::game_data()
    }

    /// An event row minted before #323 holds stage `_A` only, and the client is
    /// handed that row by `/quests` for the rest of the event's window. The
    /// `/quests` refresh gives it the missing stage at the row's own level, keeps
    /// every roll already shown, and is a no-op the second time.
    #[test]
    fn a_first_stage_only_event_row_gains_its_later_stage_on_refresh() {
        let (sd, gd) = (static_data(), game_data());
        let scaling = &sd.quests_daily.level_scaling;
        let eq24 = Uuid::parse_str(EQ24).unwrap();
        let a = Uuid::parse_str(EQ24_A).unwrap();
        let b = Uuid::parse_str(EQ24_B).unwrap();
        let row = event_quests::mint(&sd, &gd, CHAR, 16, REPORT_323_NOW)
            .into_iter()
            .find(|m| m.quest.gld_quest_id == eq24)
            .expect("EQ24 is open on 2026-10-03");
        let level = row.quest.difficulty_level;

        // What the old mint stored: the named stage only.
        let mut stored = blades_lib::util::dungeon::generate_for_dungeon_with_seed(
            &gd,
            &a,
            blades_lib::util::dungeon::run_loot_seed(&row.quest_id, 0),
            level,
            scaling.given_xp(level),
        )
        .unwrap();
        let before = stored.clone();
        assert!(
            gd.dungeons[&b]
                .spawn_info
                .enemy_spawn_groups
                .keys()
                .all(|g| !stored.enemy_generated_data.contains_key(g)),
            "precondition: the stale row has no stage _B"
        );

        let fresh = event_row_fresh(&gd, scaling, row.quest_id, &row.quest).unwrap();
        assert!(add_missing_dungeon_sections(&mut stored, &fresh), "the row is repaired");

        for group in gd.dungeons[&b].spawn_info.enemy_spawn_groups.keys() {
            let rolls = &stored.enemy_generated_data[group];
            for (i, spawner) in rolls.iter().enumerate() {
                // The row's level plus the group's APK delta (#365).
                let want =
                    (level + blades_lib::util::dungeon::spawn_group_level_delta(group, i)).max(1);
                assert!(
                    spawner
                        .iter()
                        .all(|e| e.enemy_level == want && e.given_xp == scaling.given_xp(want)),
                    "stage _B group {group} is at the row's level {level} + its delta"
                );
            }
        }
        assert_eq!(stored.chest_generated_data.len(), 2, "stage _B's two chests");
        for (group, rolls) in &before.enemy_generated_data {
            assert_eq!(
                serde_json::to_value(&stored.enemy_generated_data[group]).unwrap(),
                serde_json::to_value(rolls).unwrap(),
                "stage _A group {group} keeps what the client was shown"
            );
        }
        assert!(!add_missing_dungeon_sections(&mut stored, &fresh), "idempotent");

        // And a row minted now needs no repair at all.
        let mut current = row.dungeon.clone().unwrap();
        assert!(!add_missing_dungeon_sections(&mut current, &fresh));
    }
}

/// Report #337: job loot, job chest tiers and job enemy families, against retail.
#[cfg(test)]
mod report_337_job_drops_and_families {
    use super::jobs_gen;
    use serde_json::{json, Value};
    use std::collections::{HashMap, HashSet};
    use uuid::Uuid;

    /// 2026-10-03 05:06 UTC, the window Sephoris played his jobs in.
    const OCT3_FETCH: u64 = 1_791_004_000;
    const PRIMARY: Uuid = Uuid::from_u128(0xb2a7471e_7a44_47a6_b483_503a9e5cae3d_u128);
    const SECONDARY: Uuid = Uuid::from_u128(0xc9ad5aae_200b_48fd_860e_ad45e0349ef0_u128);

    const DUELISTS: [&str; 3] = [
        "20e856fc-9465-4ffe-8d0a-6118c2eed219", // Duelist Avenger
        "225d747b-9d24-4ffc-9ece-541728b4aef0", // Duelist Barbarian
        "878febe5-106b-4b48-972a-7debd771a079", // Duelist Swordmage
    ];
    /// Skeever, Spider (critter), Wolf, Wisp: retail secondaries only.
    const CRITTERS: [&str; 4] = [
        "31be99a6-8557-4e9b-81e6-5503f900b7d2",
        "3d932102-3b5c-42ba-b96a-35405752c5a3",
        "7f9c2b46-e6b8-4a65-9caa-f2b952623c23",
        "90a62106-6294-4456-8206-cf6817995bf8",
    ];
    const MERCENARY: &str = "4c60bb97-3918-485a-822e-1017d2401dd2";
    const WARMASTER: &str = "33de9f64-8eb6-41d2-b62d-8b7fb3632729";
    const ATRONACH: &str = "d14a0ec0-39a5-417f-a21c-8c4840d60a56"; // min level 25

    fn game_data() -> blades_lib::game_data::GameData {
        super::report85_job_generated_data_tests::game_data()
    }

    fn pools() -> Value {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/static");
        serde_json::from_str(&std::fs::read_to_string(dir.join("job_pools.json")).unwrap()).unwrap()
    }

    /// Non-Duel jobs across many characters, levels and days.
    fn ordinary_jobs() -> Vec<Value> {
        let pools = pools();
        let mut out = Vec::new();
        for day in 0..7u64 {
            let now = OCT3_FETCH + day * 86_400;
            let boundary = jobs_gen::current_reset_boundary(&pools, now);
            for c in 0..30u128 {
                let character = Uuid::from_u128(0x337_0000 + c);
                for level in [5u16, 12, 22, 40, 70, 100] {
                    for j in jobs_gen::generate(&pools, character, level, 0, boundary, now).0 {
                        if j["jobSetup"]["jobType"] != 5 {
                            out.push(j);
                        }
                    }
                }
            }
        }
        out
    }

    fn job(id: u128, seed: i64, level: i64) -> Value {
        json!({
            "questId": Uuid::from_u128(id).to_string(),
            "difficultyLevel": level,
            "seed": seed,
            "objectiveStatuses": {},
            "jobSetup": { "jobType": 0, "primaryEnemyCount": 6, "secondaryEnemyCount": 4 },
        })
    }

    fn generated(gd: &blades_lib::game_data::GameData, j: &Value) -> blades_lib::user_data::DungeonGeneratedData {
        jobs_gen::generated_data_for_job(gd, j, &crate::quest::shipped_scaling())
            .expect("the reference dungeon is in the corpus")
    }

    fn loot_of(data: &blades_lib::user_data::DungeonGeneratedData, group: Uuid) -> String {
        let enemies = &data.enemy_generated_data[&group];
        let mut tables: Vec<String> = Vec::new();
        for spawner in enemies {
            for e in spawner {
                let mut t: Vec<_> = e
                    .loot_table_loot
                    .iter()
                    .map(|(k, v)| {
                        let mut s: Vec<_> = v.stackable_items.iter().collect();
                        s.sort();
                        format!("{k}:{s:?}")
                    })
                    .collect();
                t.sort();
                tables.push(t.join(","));
            }
        }
        tables.join("|")
    }

    /// Sephoris's board: every job at a level dropped the same things from the
    /// same corpses -- secondary spawner 0 gave Major Aversion to Poison in 9 of
    /// his 9 jobs. Retail's loot differed job to job (409 distinct first-enemy
    /// loots among 417 captured jobs).
    #[test]
    fn two_jobs_at_one_level_roll_their_own_loot() {
        let gd = game_data();
        let mut primary = HashSet::new();
        let mut secondary = HashSet::new();
        for i in 0..12u128 {
            let data = generated(&gd, &job(0x337_0001 + i, 1_000 + i as i64, 22));
            primary.insert(loot_of(&data, PRIMARY));
            secondary.insert(loot_of(&data, SECONDARY));
        }
        assert!(primary.len() >= 10, "12 jobs, {} distinct primary loots", primary.len());
        assert!(secondary.len() >= 10, "12 jobs, {} distinct secondary loots", secondary.len());
    }

    /// The control: one job is described identically every time it is asked
    /// for, so the board and the stored row hand the client one dungeon.
    #[test]
    fn one_job_is_described_identically_every_time() {
        let gd = game_data();
        let j = job(0x337_00ff, -42, 22);
        let (a, b) = (generated(&gd, &j), generated(&gd, &j));
        assert_eq!(loot_of(&a, PRIMARY), loot_of(&b, PRIMARY));
        assert_eq!(loot_of(&a, SECONDARY), loot_of(&b, SECONDARY));
        let tiers = |d: &blades_lib::user_data::DungeonGeneratedData| {
            let mut t: Vec<_> = d
                .chest_generated_data
                .iter()
                .map(|(k, v)| (*k, v.iter().map(|c| c.tier).collect::<Vec<_>>()))
                .collect();
            t.sort();
            t
        };
        assert_eq!(tiers(&a), tiers(&b));
    }

    /// Retail's job chests followed the APK's two job chest cycles: over 463
    /// captured jobs the main chest was wooden/silver/gold 412/35/16 and the
    /// secret chest silver/gold/Elder 420/40/3. We sent wooden and silver on
    /// every job, so his 10 jobs held 0 gold and 0 Elder chests.
    #[test]
    fn job_chests_follow_the_job_chest_cycles() {
        let gd = game_data();
        let main = Uuid::from_u128(0x5adb41cc_d48e_4505_a62b_ac092e95bcf7);
        let secret = Uuid::from_u128(0x20640508_6ef7_466a_98ea_d79221356d97);
        let mut seen: HashMap<(Uuid, i64), u32> = HashMap::new();
        let n = 1_500u32;
        for i in 0..n {
            let data = generated(&gd, &job(0x337_1000 + i as u128, i as i64 * 7919, 22));
            for (spawn, chests) in &data.chest_generated_data {
                for c in chests {
                    *seen.entry((*spawn, c.tier)).or_default() += 1;
                }
            }
        }
        let pct = |s: Uuid, t: i64| 100.0 * *seen.get(&(s, t)).unwrap_or(&0) as f64 / n as f64;
        // Cycle: 86 / 10 / 4 per 100. Retail measured 89.0 / 7.6 / 3.5.
        assert!((2.5..=5.5).contains(&pct(main, 3)), "main gold {:.1}%", pct(main, 3));
        assert!((7.0..=12.5).contains(&pct(main, 2)), "main silver {:.1}%", pct(main, 2));
        assert!(pct(main, 1) > 80.0, "main wooden {:.1}%", pct(main, 1));
        // Cycle: 89.4 / 10 / 0.6 per 100. Retail measured 90.7 / 8.6 / 0.65.
        assert!((7.5..=12.5).contains(&pct(secret, 3)), "secret gold {:.1}%", pct(secret, 3));
        assert!((0.1..=1.5).contains(&pct(secret, 4)), "secret Elder {:.2}%", pct(secret, 4));
        assert!(pct(secret, 2) > 85.0, "secret silver {:.1}%", pct(secret, 2));
        // Nothing outside the cycles' own rarities.
        for ((spawn, tier), _) in &seen {
            assert!(
                (*spawn == main && (1..=3).contains(tier)) || (*spawn == secret && (2..=4).contains(tier)),
                "unexpected chest {spawn} tier {tier}"
            );
        }
    }

    /// Retail never put a Duelist in a job, and never led one with a critter.
    #[test]
    fn job_families_are_the_ones_retail_used() {
        let jobs = ordinary_jobs();
        assert!(jobs.len() > 1_000, "{} jobs", jobs.len());
        for j in &jobs {
            let s = &j["jobSetup"];
            let fam = |k: &str| s[k].as_str().map(str::to_string);
            for k in ["primaryEnemyFamilyId", "secondaryEnemyFamilyId", "bossEnemyFamilyId", "secretBossEnemyFamilyId"] {
                if let Some(f) = fam(k) {
                    assert!(!DUELISTS.contains(&f.as_str()), "{k} is a Duelist: {j}");
                }
            }
            for k in ["primaryEnemyFamilyId", "bossEnemyFamilyId", "secretBossEnemyFamilyId"] {
                if let Some(f) = fam(k) {
                    assert!(!CRITTERS.contains(&f.as_str()), "{k} is a critter family: {j}");
                }
            }
        }
    }

    /// Every family has a variant at the job's level (1,251 of 1,251 retail
    /// slots); Atronachs start at 25, so a level-12 job may not field one.
    #[test]
    fn job_families_fit_the_job_level() {
        for j in ordinary_jobs() {
            let s = &j["jobSetup"];
            let level = j["difficultyLevel"].as_i64().unwrap();
            let boss_level = level + s["bossLevelDelta"].as_i64().unwrap();
            for (k, at) in [
                ("primaryEnemyFamilyId", level),
                ("secondaryEnemyFamilyId", level),
                ("bossEnemyFamilyId", boss_level),
            ] {
                let f = s[k].as_str().unwrap();
                let family = jobs_gen::JOB_FAMILIES
                    .iter()
                    .find(|x| x.id == f)
                    .unwrap_or_else(|| panic!("{k} {f} is not a job family"));
                assert!(
                    (family.min_level..=family.max_level).contains(&at),
                    "{k} {f} has no variant at level {at}"
                );
                if at < 25 {
                    assert_ne!(f, ATRONACH);
                }
            }
            // And the boss comes from the primary's or the secondary's boss list.
            let bosses = |k: &str| {
                let id = s[k].as_str().unwrap();
                jobs_gen::JOB_FAMILIES.iter().find(|x| x.id == id).unwrap().bosses
            };
            let boss = s["bossEnemyFamilyId"].as_str().unwrap();
            assert!(
                bosses("primaryEnemyFamilyId").contains(&boss)
                    || bosses("secondaryEnemyFamilyId").contains(&boss)
                    || boss == s["primaryEnemyFamilyId"].as_str().unwrap(),
                "boss {boss} is in neither boss list: {j}"
            );
        }
    }

    /// Mercenaries and Warmasters lead jobs (16 and 16 retail primaries), every
    /// variant of both weak to Poison, and a Mercenary job's boss is usually one
    /// of them.
    #[test]
    fn mercenary_jobs_still_happen_and_bring_their_bosses() {
        let jobs = ordinary_jobs();
        let merc: Vec<_> = jobs
            .iter()
            .filter(|j| j["jobSetup"]["primaryEnemyFamilyId"] == MERCENARY)
            .collect();
        assert!(merc.len() >= 20, "{} Mercenary-led jobs", merc.len());
        let own = merc
            .iter()
            .filter(|j| {
                let b = &j["jobSetup"]["bossEnemyFamilyId"];
                *b == MERCENARY || *b == WARMASTER
            })
            .count();
        // Retail: 12 of 16.
        assert!(own * 2 > merc.len(), "{own} of {} Mercenary jobs kept a human boss", merc.len());
    }
}

/// Event rows minted before the container / level fix heal on the next `/quests`
/// (tracker #353, #358, #365). An event row is durable for its 48-hour window and
/// the client draws every container from it, so the generator fix alone would
/// leave this window's players with empty containers.
#[cfg(test)]
mod report365_event_row_heal_tests {
    use super::*;

    /// EQ30 "Spirit of the Hunt", HauDrauf's 10-08 event; never captured.
    const EQ30: &str = "11ffa10c-f587-432b-8901-fde27f37f65a";

    fn row_and_fresh(level: i64) -> (DungeonGeneratedData, DungeonGeneratedData) {
        let gd = super::report85_job_generated_data_tests::game_data();
        let scaling = blades_lib::static_data::QuestLevelScaling::default();
        let (mut info, _) =
            generate_quest_data(&gd, Uuid::parse_str(EQ30).unwrap(), level, &scaling).unwrap();
        info.r#type = blades_lib::user_data::QuestType::GameEvent;
        info.difficulty_level = level;
        let row_id = Uuid::from_u128(0x365);
        let fresh = event_row_fresh(&gd, &scaling, row_id, &info).expect("EQ30 generates");
        // The row as fork/main minted it: one result per container spawn, every
        // enemy flat at the row's difficulty.
        let mut stale = fresh.clone();
        for pile in stale.item_generated_data.values_mut() {
            pile.truncate(1);
        }
        for e in stale.enemy_generated_data.values_mut().flatten().flatten() {
            e.enemy_level = level;
            e.given_xp = scaling.given_xp(level);
        }
        (stale, fresh)
    }

    #[test]
    fn a_minted_row_gets_its_missing_containers_and_keeps_the_ones_it_had() {
        let (mut stored, fresh) = row_and_fresh(78);
        let before = stored.clone();
        assert!(grow_short_item_piles(&mut stored, &fresh));
        let mut sizes: Vec<usize> = stored.item_generated_data.values().map(Vec::len).collect();
        sizes.sort();
        assert_eq!(sizes, vec![1, 3, 7], "EQ30's 7 / 3 / 1 breakables");
        for (spawn, pile) in &before.item_generated_data {
            assert_eq!(
                serde_json::to_value(&pile[0]).unwrap(),
                serde_json::to_value(&stored.item_generated_data[spawn][0]).unwrap(),
                "a container the player may have opened is untouched"
            );
        }
        assert!(!grow_short_item_piles(&mut stored, &fresh), "idempotent");
    }

    #[test]
    fn a_minted_row_gets_its_bosses_six_levels_up() {
        let (mut stored, fresh) = row_and_fresh(78);
        assert!(relevel_event_enemies(&mut stored, &fresh));
        for e in stored.enemy_generated_data.values().flatten().flatten() {
            assert_eq!(e.enemy_level, 84);
        }
        assert!(!relevel_event_enemies(&mut stored, &fresh), "idempotent");
    }

    /// CONTROL: a row this server did not generate is the player's captured state.
    #[test]
    fn a_captured_row_is_left_alone() {
        let (mut stored, fresh) = row_and_fresh(78);
        stored.version = 1;
        let before = serde_json::to_value(&stored).unwrap();
        assert!(!grow_short_item_piles(&mut stored, &fresh));
        assert!(!relevel_event_enemies(&mut stored, &fresh));
        assert_eq!(serde_json::to_value(&stored).unwrap(), before);
    }
}
