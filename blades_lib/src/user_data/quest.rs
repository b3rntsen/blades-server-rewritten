use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Serialize, Deserialize, Debug, Copy, Clone)]
#[serde(rename_all = "UPPERCASE")]
pub enum QuestType {
    Normal,
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone)]
#[serde(rename_all = "PascalCase")]
pub enum QuestStatus {
    Active,
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
