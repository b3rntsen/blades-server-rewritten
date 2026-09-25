//! Newblades content: the registry of everything we add that retail never had.
//!
//! The owner's rule (requests #197/#198) is that new content must always be
//! recognisable as new: its own dungeon, a visible `Newblades:` prefix on its
//! name, and never mistaken for a measurement of retail. This module is the one
//! place the server answers "is this ours?".
//!
//! Two properties keep retail fidelity the default:
//!
//! * `parsed.json` stays pure APK output. Spawn groups a Newblades dungeon adds
//!   live in `newblades_content.json` and are merged in at load by [`apply`];
//!   regenerating `parsed.json` from the APK can therefore never drop them, and
//!   every test that compares `parsed.json` or the capture corpus with retail
//!   keeps comparing retail with retail.
//! * Code that asserts a retail shape (event calendars, quest lists, reward
//!   curves) excludes content by meaning with [`NewbladesContent::is_newblades_quest`]
//!   instead of by name.
//!
//! [`apply`]: NewbladesContent::apply

use std::collections::HashMap;

use serde::Deserialize;
use uuid::Uuid;

use super::{GameData, GameDataEnemySpawnGroup};

/// The label every piece of new content carries in its player-facing name.
pub const PREFIX: &str = "Newblades:";

#[derive(Deserialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct NewbladesContent {
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub content: Vec<ContentEntry>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ContentEntry {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub quest_ids: Vec<Uuid>,
    #[serde(default)]
    pub dungeon_ids: Vec<Uuid>,
    #[serde(default)]
    pub enemy_data_ids: Vec<Uuid>,
    /// Loc key -> text as shipped in the client pack (`{retail}` = the stock text).
    #[serde(default)]
    pub loc: HashMap<String, String>,
    /// dungeon id -> (new spawn-group id -> quantity). Groups the client-side
    /// dungeon definition gains; the server must know them to send their
    /// `enemyGeneratedData`.
    #[serde(default)]
    pub extra_spawn_groups: HashMap<Uuid, HashMap<Uuid, u64>>,
}

impl NewbladesContent {
    pub fn is_newblades_quest(&self, quest_id: &Uuid) -> bool {
        self.content.iter().any(|c| c.quest_ids.contains(quest_id))
    }

    pub fn is_newblades_dungeon(&self, dungeon_id: &Uuid) -> bool {
        self.content.iter().any(|c| c.dungeon_ids.contains(dungeon_id))
    }

    /// Every rule the registry must satisfy against the retail data it extends.
    pub fn validate(&self, game_data: &GameData) -> Result<(), String> {
        if self.prefix != PREFIX {
            return Err(format!("prefix is {:?}, must be {PREFIX:?}", self.prefix));
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.content {
            if !c.id.starts_with("nb-") {
                return Err(format!("content id {:?} must start with \"nb-\"", c.id));
            }
            if !seen.insert(c.id.as_str()) {
                return Err(format!("content id {:?} is listed twice", c.id));
            }
            if c.quest_ids.is_empty() && c.dungeon_ids.is_empty() && c.enemy_data_ids.is_empty() {
                return Err(format!("{}: names nothing", c.id));
            }
            if !c.quest_ids.is_empty() && c.loc.is_empty() {
                return Err(format!("{}: a quest with no {PREFIX} name", c.id));
            }
            for (key, text) in &c.loc {
                if !text.starts_with(PREFIX) {
                    return Err(format!("{}: loc {key} = {text:?} lacks the {PREFIX} prefix", c.id));
                }
            }
            for q in &c.quest_ids {
                // The client can only offer a quest whose template it knows, and
                // parsed.json is the server's copy of that template list.
                if !game_data.quests.contains_key(q) {
                    return Err(format!("{}: quest {q} is not a client quest template", c.id));
                }
            }
            for d in &c.dungeon_ids {
                if !game_data.dungeons.contains_key(d) {
                    return Err(format!("{}: dungeon {d} is not a client dungeon", c.id));
                }
            }
            for (d, groups) in &c.extra_spawn_groups {
                if !c.dungeon_ids.contains(d) {
                    return Err(format!("{}: adds groups to {d}, which it does not own", c.id));
                }
                let Some(dungeon) = game_data.dungeons.get(d) else {
                    return Err(format!("{}: dungeon {d} is not a client dungeon", c.id));
                };
                for g in groups.keys() {
                    if dungeon.spawn_info.enemy_spawn_groups.contains_key(g) {
                        return Err(format!("{}: group {g} is already a retail group of {d}", c.id));
                    }
                }
            }
        }
        Ok(())
    }

    /// Merge the added spawn groups into `game_data`. Validates first and
    /// changes nothing on failure. Returns how many groups were added.
    pub fn apply(&self, game_data: &mut GameData) -> Result<usize, String> {
        self.validate(game_data)?;
        let mut added = 0;
        for c in &self.content {
            for (d, groups) in &c.extra_spawn_groups {
                let dungeon = game_data.dungeons.get_mut(d).expect("validated");
                for (g, quantity) in groups {
                    dungeon
                        .spawn_info
                        .enemy_spawn_groups
                        .insert(*g, GameDataEnemySpawnGroup { quantity: *quantity });
                    added += 1;
                }
            }
        }
        Ok(added)
    }
}

/// Read `path` (optional) and merge its spawn groups into `game_data`.
///
/// `Ok(None)` when the file is absent ("no new content"). An unreadable or
/// invalid file is an `Err` and changes nothing, so the caller can log it and
/// carry on with pure retail data: a bad edit can never fail a start.
pub fn load(
    path: &std::path::Path,
    game_data: &mut GameData,
) -> Result<Option<(NewbladesContent, usize)>, String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let reg: NewbladesContent =
        serde_json::from_str(&raw).map_err(|e| format!("{path:?} is not valid: {e}"))?;
    let added = reg.apply(game_data).map_err(|e| format!("{path:?} refused: {e}"))?;
    Ok(Some((reg, added)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed() -> NewbladesContent {
        serde_json::from_str(include_str!("../../../deploy/static/newblades_content.json"))
            .expect("newblades_content.json parses")
    }

    fn parsed() -> GameData {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/parsed.json");
        serde_json::from_str(&std::fs::read_to_string(p).expect("read parsed.json"))
            .expect("parse parsed.json")
    }

    #[test]
    fn the_committed_registry_is_valid_against_the_client_data() {
        let reg = committed();
        assert!(!reg.content.is_empty());
        reg.validate(&parsed()).expect("registry valid");
    }

    #[test]
    fn the_proof_quest_is_flagged_and_a_retail_quest_is_not() {
        let reg = committed();
        let eq15: Uuid = "e8f3614c-8672-4f77-9dad-4b400676f4b6".parse().unwrap();
        // CONTROL: EQ14, the other event open alongside it, is retail.
        let eq14: Uuid = "61f7a386-dd0c-4119-acf6-7f41a9c5b62b".parse().unwrap();
        assert!(reg.is_newblades_quest(&eq15));
        assert!(!reg.is_newblades_quest(&eq14));
    }

    fn entry(json: &str) -> NewbladesContent {
        serde_json::from_str(&format!(r#"{{"prefix":"Newblades:","content":[{json}]}}"#)).unwrap()
    }

    // EQ18 is the never-released placeholder event: a client quest and dungeon
    // with zero enemy groups, the slot the doc proposes for real new content.
    const EQ18_QUEST: &str = "1a38ac88-2b00-47a4-a626-cb2e8af2ab1f";
    const EQ18_DUNGEON: &str = "57da57d2-4ca4-43a1-ac87-26801fe602d0";

    #[test]
    fn apply_adds_groups_to_the_owned_dungeon_only() {
        let mut gd = parsed();
        let d: Uuid = EQ18_DUNGEON.parse().unwrap();
        let before = gd.dungeons[&d].spawn_info.enemy_spawn_groups.len();
        let total_before: usize = gd.dungeons.values().map(|x| x.spawn_info.enemy_spawn_groups.len()).sum();
        let reg = entry(&format!(
            r#"{{"id":"nb-t","status":"test","dungeonIds":["{EQ18_DUNGEON}"],
                "extraSpawnGroups":{{"{EQ18_DUNGEON}":{{"00000000-0000-4000-8000-00000000d001":2}}}}}}"#
        ));
        assert_eq!(reg.apply(&mut gd), Ok(1));
        assert_eq!(gd.dungeons[&d].spawn_info.enemy_spawn_groups.len(), before + 1);
        let total_after: usize = gd.dungeons.values().map(|x| x.spawn_info.enemy_spawn_groups.len()).sum();
        assert_eq!(total_after, total_before + 1, "no other dungeon changed");
    }

    #[test]
    fn a_retail_group_id_cannot_be_redefined() {
        let mut gd = parsed();
        // EQ15's own 'Skeletons' group: retail, so the registry may not claim it.
        let reg = entry(
            r#"{"id":"nb-t","status":"test","dungeonIds":["924f1147-fd7f-4736-9e2d-f33fa942dbdd"],
                "extraSpawnGroups":{"924f1147-fd7f-4736-9e2d-f33fa942dbdd":{"b98ac1b1-ab3d-4455-bf03-77a5be53a358":9}}}"#,
        );
        let err = reg.apply(&mut gd).unwrap_err();
        assert!(err.contains("already a retail group"), "{err}");
        let d: Uuid = "924f1147-fd7f-4736-9e2d-f33fa942dbdd".parse().unwrap();
        let g: Uuid = "b98ac1b1-ab3d-4455-bf03-77a5be53a358".parse().unwrap();
        assert_eq!(gd.dungeons[&d].spawn_info.enemy_spawn_groups[&g].quantity, 2, "unchanged");
    }

    #[test]
    fn a_quest_name_without_the_prefix_is_refused() {
        let gd = parsed();
        let reg = entry(&format!(
            r#"{{"id":"nb-t","status":"test","questIds":["{EQ18_QUEST}"],"loc":{{"EQ18_Title":"The Draugr Barrow"}}}}"#
        ));
        assert!(reg.validate(&gd).unwrap_err().contains("prefix"));
        // CONTROL: the same entry with the prefix passes.
        let ok = entry(&format!(
            r#"{{"id":"nb-t","status":"test","questIds":["{EQ18_QUEST}"],"loc":{{"EQ18_Title":"Newblades: The Draugr Barrow"}}}}"#
        ));
        ok.validate(&gd).expect("prefixed entry is valid");
    }

    #[test]
    fn ids_must_carry_the_nb_prefix_and_quests_must_be_client_templates() {
        let gd = parsed();
        let bad_id = entry(r#"{"id":"draugr","status":"t","enemyDataIds":["67d9e5a6-191c-4c8f-863e-4c608d7cf3d3"]}"#);
        assert!(bad_id.validate(&gd).unwrap_err().contains("nb-"));
        let unknown = entry(
            r#"{"id":"nb-t","status":"t","questIds":["00000000-0000-4000-8000-000000000001"],"loc":{"X":"Newblades: x"}}"#,
        );
        assert!(unknown.validate(&gd).unwrap_err().contains("not a client quest template"));
    }

    #[test]
    fn an_absent_file_means_no_content() {
        let reg = NewbladesContent::default();
        assert!(!reg.is_newblades_quest(&Uuid::nil()));
    }
    #[test]
    fn load_is_optional_and_a_bad_file_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("nbc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut gd = parsed();
        assert_eq!(load(&dir.join("absent.json"), &mut gd).map(|r| r.is_none()), Ok(true));
        let bad = dir.join("bad.json");
        // Valid JSON, invalid content: adds to EQ18 but the id lacks "nb-".
        std::fs::write(&bad, format!(
            r#"{{"prefix":"Newblades:","content":[{{"id":"x","status":"t","dungeonIds":["{EQ18_DUNGEON}"],
                "extraSpawnGroups":{{"{EQ18_DUNGEON}":{{"00000000-0000-4000-8000-00000000d001":2}}}}}}]}}"#
        )).unwrap();
        assert!(load(&bad, &mut gd).is_err());
        let d: Uuid = EQ18_DUNGEON.parse().unwrap();
        assert!(gd.dungeons[&d].spawn_info.enemy_spawn_groups.is_empty(), "nothing merged");
        let _ = std::fs::remove_dir_all(&dir);
    }

}
