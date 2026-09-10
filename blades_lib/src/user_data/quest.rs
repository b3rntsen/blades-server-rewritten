use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::economy::RewardGrant;

/// The `type` discriminator on a quest.
///
/// All three values are taken from the pre-shutdown corpus, where the counts over
/// retail `/quests`, `/accept`, `/objectives` and `/complete` bodies are
/// `NORMAL` 2332, `GAME_EVENT` 2753, `JOB` 285. `JOB` is modelled here for
/// completeness — the job board is still served as raw `Value` — but `GAME_EVENT`
/// is load-bearing: without it an event quest cannot be told apart from an ordinary
/// one, and the two are paid differently (see [`Quest::rewards`]).
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
pub enum QuestType {
    #[serde(rename = "NORMAL")]
    Normal,
    #[serde(rename = "GAME_EVENT")]
    GameEvent,
    #[serde(rename = "JOB")]
    Job,
}

/// Objective status.
///
/// `Completed` is not an embellishment: the client reports objective completion as
/// `{"status":"Completed","progress":1.0}` and an enum that only knew `Active`
/// rejected the whole body. 137 of the 139 `/objectives` calls ever made against
/// this server answered
/// `400 Json deserialize error: unknown variant 'Completed', expected 'Active'`,
/// which means no quest could be finished through the normal flow at all.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum QuestStatus {
    Active,
    Completed,
}

/// The `gameEventQuestData` block an event quest carries: which event instance it
/// belongs to, as `"<eventId>::<startTimeSecs>"`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GameEventQuestData {
    pub game_event_instance_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ObjectiveStatus {
    pub status: QuestStatus,
    pub progress: f64,
    pub completed: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Quest {
    pub version: u64,
    pub r#type: QuestType,
    pub objective_statuses: HashMap<Uuid, ObjectiveStatus>,
    pub difficulty_level: i64,
    /// Retail's quest seed does not fit ANY fixed integer type, so this field
    /// stores the JSON number verbatim and never narrows it.
    ///
    /// It has been narrowed twice, and each narrowing took prod down in the
    /// opposite direction from the last:
    ///
    /// * `u64` rejected `-1785270870` — a signed seed from a captured transfer
    ///   payload — and one such quest failed the entire `import-character` body,
    ///   so the player could not transfer at all (report #59).
    /// * `i64`, the fix for that, then rejected `13753969001480220957` — which is
    ///   above `i64::MAX` and is what the live database actually holds. The
    ///   `GET /characters/<id>/quests` route 500ed, and because the client
    ///   requests quests last in its load sequence, the game hung on the loading
    ///   spinner with no error. 71 of 205 stored seeds are in that range.
    ///
    /// Both populations are real: signed values arrive in captured payloads,
    /// above-`i64::MAX` values sit in Postgres. A single integer type cannot hold
    /// both without reinterpreting the bits, and reinterpreting would silently
    /// rewrite a player's stored seed into a different number.
    ///
    /// **Nothing in the server reads this value** — it is carried and persisted,
    /// never computed with. So the correct type is the one that round-trips
    /// exactly what arrived, whatever that was. `serde_json::Number` does that for
    /// both signs and the full unsigned range. If a consumer ever needs an
    /// integer, it must handle both populations explicitly at that call site
    /// rather than pushing the narrowing back down here.
    pub seed: serde_json::Number,
    pub gld_quest_id: Uuid,
    /// Which event instance this quest belongs to. Present on, and only on, a
    /// `GAME_EVENT` quest (2753 of 2753 captured event quests carry it; no `NORMAL`
    /// or `JOB` quest ever does), so it is skipped when absent rather than sent as
    /// `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_event_quest_data: Option<GameEventQuestData>,
    /// The five milestone rewards an event quest pays, in order. The Nth completion
    /// of the instance pays `rewards[N]`; the last one additionally pays
    /// [`Self::final_reward`].
    ///
    /// The tier order is measured, not assumed: across 93 retail instances the
    /// observed payout matched `rewards[completion_index]` 91/93 times on the first
    /// completion, 67/68 on the second, 59/60 on the third, 56/57 on the fourth, and
    /// every one of the 54 observed fifth completions paid `rewards[4]` merged with
    /// `finalReward`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewards: Option<Vec<RewardGrant>>,
    /// The one-off bonus paid alongside the last milestone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_reward: Option<RewardGrant>,
    /// What a town JOB pays when it is completed, captured off the job's own
    /// `jobSetup` at the moment the board was rolled.
    ///
    /// A job needs its own slot because a job's `gldQuestId` is our sentinel: it is
    /// in neither `quest_rewards.json` (keyed by real template ids) nor
    /// `event_quests.json`, so `resolve_completion_reward` fell through every branch
    /// and paid **nothing** for every job ever completed on this server.
    ///
    /// Retail's own numbers, matched job-for-job in the corpus: job
    /// `1385706b-...`'s `jobSetup` declared `rewardXp: 1526` and
    /// `rewardItemCount: 1000` of `rewardItemId: f8d27767-...` (gold), and its
    /// `/complete` response paid exactly
    /// `{"currencies":{"f8d27767-...":1000},"characterXp":1526}`; job `9ba20667-...`
    /// declared 1586/1000 and paid 1586/1000. `rewardItemId` is gold on 802 of 802
    /// sampled jobs, and it arrives as a **currency**, not a stackable -- 5 of 60
    /// sampled non-template completions have that gold-plus-XP-only shape and 0 of
    /// 20 story-quest completions do, which is what tells the two apart.
    ///
    /// `None` on every non-job quest (and on job rows written before this field
    /// existed, which fall back to XP alone), so it never appears on the wire for
    /// anything retail sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_reward: Option<RewardGrant>,
    pub completed: bool,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct QuestWithId {
    pub quest_id: Uuid,
    #[serde(flatten)]
    pub quest: Quest,
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Report #59: a character transfer died with
    /// `Json deserialize error: invalid value: integer -1785270870, expected u64`.
    ///
    /// Retail's quest seed is signed and often negative. `Quest.seed` was the only
    /// `u64` seed in the codebase, so ONE such quest failed the whole
    /// `import-character` body and the player could not transfer at all.
    ///
    /// The value below is the exact one from his error.
    #[test]
    fn a_negative_retail_seed_deserializes() {
        let q: QuestWithId = serde_json::from_value(serde_json::json!({
            "questId": "159bc1e7-454c-4e2a-90cf-e200c74b961a",
            "version": 2,
            "type": "NORMAL",
            "objectiveStatuses": {},
            "difficultyLevel": -1,
            "seed": -1785270870i64,
            "gldQuestId": "159bc1e7-454c-4e2a-90cf-e200c74b961a",
            "completed": false,
        }))
        .expect("a negative seed is normal retail data and must not fail the import");
        // Compared through JSON rather than against a typed literal: a negative
        // literal would not COMPILE against a u64 field, and a compile error is
        // weaker evidence than watching the deserialize itself fail.
        let back = serde_json::to_value(&q).unwrap();
        assert_eq!(back["seed"], serde_json::json!(-1785270870i64));
    }

    /// It must round-trip unchanged: casting a negative seed through `u64` would
    /// hand the client a huge positive number instead of the value retail used.
    #[test]
    fn a_negative_seed_round_trips() {
        let src = serde_json::json!({
            "questId": "159bc1e7-454c-4e2a-90cf-e200c74b961a",
            "version": 2,
            "type": "NORMAL",
            "objectiveStatuses": {},
            "difficultyLevel": -1,
            "seed": -1785270870i64,
            "gldQuestId": "159bc1e7-454c-4e2a-90cf-e200c74b961a",
            "completed": false,
        });
        let q: QuestWithId = serde_json::from_value(src).unwrap();
        let back = serde_json::to_value(&q).unwrap();
        assert_eq!(back["seed"], serde_json::json!(-1785270870i64));
    }

    /// Positive seeds, which most captured quests carry, still work.
    #[test]
    fn a_positive_seed_still_works() {
        let q: QuestWithId = serde_json::from_value(serde_json::json!({
            "questId": "159bc1e7-454c-4e2a-90cf-e200c74b961a",
            "version": 2, "type": "NORMAL", "objectiveStatuses": {},
            "difficultyLevel": -1, "seed": 485975867,
            "gldQuestId": "159bc1e7-454c-4e2a-90cf-e200c74b961a", "completed": false,
        }))
        .unwrap();
        let back = serde_json::to_value(&q).unwrap();
        assert_eq!(back["seed"], serde_json::json!(485975867i64));
    }

    fn quest_with_seed(seed: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "questId": "159bc1e7-454c-4e2a-90cf-e200c74b961a",
            "version": 2, "type": "NORMAL", "objectiveStatuses": {},
            "difficultyLevel": -1, "seed": seed,
            "gldQuestId": "159bc1e7-454c-4e2a-90cf-e200c74b961a", "completed": false,
        })
    }

    /// The live outage this replaces. `13753969001480220957` is the exact value
    /// from the production error log, and it is what the database holds for the
    /// quest that hung the reporter's game on the loading spinner.
    ///
    /// It is above `i64::MAX` (9223372036854775807), so the `i64` that fixed the
    /// negative case rejected it — and `/quests` is the LAST call in the client's
    /// load sequence, so a 500 there shows up as an infinite spinner rather than
    /// an error message.
    #[test]
    fn a_seed_above_i64_max_deserializes() {
        let q: QuestWithId =
            serde_json::from_value(quest_with_seed(serde_json::json!(13753969001480220957u64)))
                .expect("an above-i64::MAX seed is what the live DB holds and must not fail");
        let back = serde_json::to_value(&q).unwrap();
        assert_eq!(back["seed"], serde_json::json!(13753969001480220957u64));
    }

    /// Both populations at once — the property that neither integer type has.
    /// A fix that only widens to `u64` passes the test above and fails this one;
    /// the `i64` it replaces does the reverse. Nothing catches both except a type
    /// that stops narrowing.
    #[test]
    fn both_seed_populations_survive_the_same_build() {
        // The signed value from report #59, and the unsigned one from today.
        for seed in [
            serde_json::json!(-1785270870i64),
            serde_json::json!(13753969001480220957u64),
            serde_json::json!(11891572268885817404u64), // also in the live log
            serde_json::json!(485975867i64),
            serde_json::json!(0i64),
        ] {
            let q: QuestWithId = serde_json::from_value(quest_with_seed(seed.clone()))
                .unwrap_or_else(|e| panic!("seed {seed} must deserialize: {e}"));
            let back = serde_json::to_value(&q).unwrap();
            assert_eq!(
                back["seed"], seed,
                "seed {seed} must round-trip byte-identically — rewriting a stored \
                 seed changes which quest the player generated",
            );
        }
    }

    /// Corpus preflight against REAL production rows.
    ///
    /// Every fixture above is hand-written, which means every one of them encodes
    /// what I *believed* retail sends. That belief has been wrong three times this
    /// year, each time taking character import or the whole load sequence down. So
    /// this one reads actual stored rows instead.
    ///
    /// Inert by default — no player data lives in this repo. To run it, dump rows
    /// from the arena database and point the env var at the file:
    ///
    /// ```sql
    /// SELECT json_agg(row_to_json(t)) FROM (
    ///   SELECT id AS "questId", info FROM quests WHERE info ? 'seed' LIMIT 500
    /// ) t;
    /// ```
    /// ```sh
    /// NB_QUEST_CORPUS=/tmp/quests.json cargo test -p blades_lib quest_corpus -- --nocapture
    /// ```
    ///
    /// Verified 2026-08-23 against 40 rows carrying seeds above `i64::MAX`,
    /// including `13753969001480220957` — the exact value whose rejection hung the
    /// game on the loading spinner.
    #[test]
    fn quest_corpus_from_production_deserializes() {
        let Ok(path) = std::env::var("NB_QUEST_CORPUS") else {
            eprintln!("NB_QUEST_CORPUS unset — corpus preflight skipped");
            return;
        };
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read corpus {path}: {e}"));
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&raw).expect("corpus must be a JSON array");
        assert!(!rows.is_empty(), "an empty corpus proves nothing — check the query");

        let mut failures = Vec::new();
        let mut above_i64 = 0usize;
        for row in &rows {
            let info = row.get("info").unwrap_or(row);
            if let Some(seed) = info.get("seed").and_then(|s| s.as_u64()) {
                if seed > i64::MAX as u64 {
                    above_i64 += 1;
                }
            }
            match serde_json::from_value::<Quest>(info.clone()) {
                Ok(q) => {
                    // Round-trip: a stored seed must come back byte-identical, or we
                    // would rewrite which quest the player actually generated.
                    let back = serde_json::to_value(&q).unwrap();
                    if back["seed"] != info["seed"] {
                        failures.push(format!(
                            "seed rewritten: {} -> {}",
                            info["seed"], back["seed"]
                        ));
                    }
                }
                Err(e) => failures.push(format!(
                    "quest {} failed: {e}",
                    row.get("questId").unwrap_or(&serde_json::Value::Null)
                )),
            }
        }

        eprintln!(
            "corpus: {} rows, {above_i64} with a seed above i64::MAX",
            rows.len()
        );
        // Non-vacuity: a corpus with no out-of-range seeds would pass against the
        // very build that broke production, so say so rather than report success.
        assert!(
            above_i64 > 0,
            "this corpus contains no seed above i64::MAX, so it does not exercise \
             the bug it exists to catch — widen the query",
        );
        assert!(failures.is_empty(), "{} row(s) failed:\n{}", failures.len(), failures.join("\n"));
    }

    /// `u64::MAX` and `i64::MIN`, the two ends. Guards against a future "tidy this
    /// up into an untagged enum" that quietly loses one boundary.
    #[test]
    fn the_extremes_of_both_ranges_round_trip() {
        for seed in [
            serde_json::json!(u64::MAX),
            serde_json::json!(i64::MIN),
        ] {
            let q: QuestWithId =
                serde_json::from_value(quest_with_seed(seed.clone())).unwrap();
            assert_eq!(serde_json::to_value(&q).unwrap()["seed"], seed);
        }
    }
}
