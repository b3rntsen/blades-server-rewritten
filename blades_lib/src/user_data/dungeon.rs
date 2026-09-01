use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;
use std::{collections::{HashMap, HashSet}, fmt};

use crate::user_data::{B64EncodedData, Items};

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LootTableResult {
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    #[serde(default)]
    pub stackable_items: HashMap<Uuid, u64>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    #[serde(default)]
    pub currencies: HashMap<Uuid, u64>,
    #[serde(skip_serializing_if = "Items::is_empty")]
    #[serde(default)]
    pub item: Items,
}

impl LootTableResult {
    pub fn merge(&mut self, other: LootTableResult) {
        for (uuid, amount) in other.stackable_items {
            self.stackable_items.insert(
                uuid,
                self.stackable_items.get(&uuid).map(|x| *x).unwrap_or(0) + amount,
            );
        }
        for (uuid, amount) in other.currencies {
            self.currencies.insert(
                uuid,
                self.currencies.get(&uuid).map(|x| *x).unwrap_or(0) + amount,
            );
        }
        self.item.0.extend(other.item.0);
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DungeonEnemyResult {
    pub enemy_level: i64,
    #[serde(rename = "givenXP")]
    pub given_xp: u64,
    //TODO: need to find a filled spawn_group_loot to verify it really is that.
    ///
    /// Both maps are OMITTED by retail when empty, so both must default or one rare
    /// object kills the whole character import (report #61). Measured over 1,045
    /// captured `/quests` bodies: of 68,683 enemy results, 190 omit `lootTableLoot`.
    /// `spawnGroupLoot` was present on all of them, but it is the same shape from the
    /// same generator and is defaulted for the same reason — the cost of defaulting a
    /// collection that is always sent is nil; the cost of not defaulting one that is
    /// occasionally omitted is a player who cannot transfer at all.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub spawn_group_loot: HashMap<Uuid, LootTableResult>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub loot_table_loot: HashMap<Uuid, LootTableResult>,
}

impl DungeonEnemyResult {
    pub fn merged_loot_table(&self) -> LootTableResult {
        let mut result = LootTableResult::default();
        for loot_table in self
            .spawn_group_loot
            .values()
            .chain(self.loot_table_loot.values())
        {
            result.merge(loot_table.clone());
        }
        result
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DungeonItemResult {
    /// THE field from report #61's error. Retail sends `{}` for an item result that
    /// generated no loot: 724 of 70,513 captured item results omit it entirely.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub loot_table_loot: HashMap<Uuid, LootTableResult>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChestGeneratedData {
    pub tier: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DungeonGeneratedData {
    //TODO: figure what the two level of depth are used for (one is named "spawner"(id) and the second "enemy"(id))
    ///
    /// A dungeon with no chests sends no `chestGeneratedData`, and one with no ground
    /// items sends no `itemGeneratedData` — 347 and 577 respectively of 6,564 captured
    /// dungeon bodies. Required fields here were two more import-killers waiting behind
    /// the one that was reported.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub enemy_generated_data: HashMap<Uuid, Vec<Vec<DungeonEnemyResult>>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub item_generated_data: HashMap<Uuid, Vec<DungeonItemResult>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub chest_generated_data: HashMap<Uuid, Vec<ChestGeneratedData>>,
    pub algorithm_version: u64,
    pub version: u64,
}

impl DungeonGeneratedData {
    pub fn get_enemy(&self, index: &EnemyIndex) -> Option<&DungeonEnemyResult> {
        self.enemy_generated_data
            .get(&index.spawner_uuid)
            .and_then(|spawner_data| spawner_data.get(index.spawner_index))
            .and_then(|enemy_data| enemy_data.get(index.enemy_index))
    }

    pub fn get_chest(&self, spawn_group_id: &Uuid, spawn_group_index: usize) -> Option<&ChestGeneratedData> {
        self.chest_generated_data
            .get(spawn_group_id)
            .and_then(|chests| chests.get(spawn_group_index))
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DungeonGeneratedDataWithId {
    pub quest_id: Uuid,
    #[serde(flatten)]
    pub inner: DungeonGeneratedData,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EnemyStatus {
    pub spawn_group_id: Uuid,
    pub xp_reward: u64,
    pub killed: bool,
    pub time: u64,
    pub loot: LootTableResult,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DungeonStatus {
    pub dungeon_settings_ids: Vec<Uuid>,
    pub revive_count: u64,
    pub level: u64,
    pub seed: i64,
    pub current_state: B64EncodedData,
    pub algorithm_version: i64,
    pub version: i64,
    #[serde(default)]
    pub enemy_status: HashMap<EnemyIndex, EnemyStatus>,
    pub collected_chests: HashSet<Uuid>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DungeonState {
    pub dungeon_status: DungeonStatus,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnemyIndex {
    pub spawner_uuid: Uuid,
    pub spawner_index: usize,
    pub enemy_index: usize,
}

impl EnemyIndex {
    pub fn new(spawner_uuid: Uuid, spawner_index: usize, enemy_index: usize) -> Self {
        Self {
            spawner_uuid,
            spawner_index,
            enemy_index,
        }
    }
}

impl fmt::Display for EnemyIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}-{}-{}",
            self.spawner_uuid, self.spawner_index, self.enemy_index
        )
    }
}

// Serialize as a single string “uuid-index-index”
impl Serialize for EnemyIndex {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

// Deserialize from that string format
impl<'de> Deserialize<'de> for EnemyIndex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        // Split from the right so that UUID (which may contain dashes) stays intact.
        let parts: Vec<&str> = s.rsplitn(3, '-').collect();
        if parts.len() != 3 {
            return Err(serde::de::Error::custom("Invalid EnemyIndex format"));
        }
        let enemy_index = parts[0]
            .parse::<usize>()
            .map_err(serde::de::Error::custom)?;
        let spawner_index = parts[1]
            .parse::<usize>()
            .map_err(serde::de::Error::custom)?;
        let spawner_uuid = Uuid::parse_str(parts[2]).map_err(serde::de::Error::custom)?;
        Ok(EnemyIndex {
            spawner_uuid,
            spawner_index,
            enemy_index,
        })
    }
}

#[cfg(test)]
mod report61_tests {
    use super::*;

    /// Report #61 (Mɾʂιɾι): a character transfer died with
    /// `missing field ` + "`lootTableLoot`" + ` at line 1 column 237933`.
    ///
    /// Retail omits an empty loot map rather than sending `{}`. One such object
    /// anywhere in a 237 KB payload failed the ENTIRE import, so the player could
    /// not transfer at all — the same failure shape as the quest-seed bugs before
    /// it (#59, and the spinner outage), and the third of its kind.
    ///
    /// Counts below are measured over 1,045 captured retail `/quests` bodies.
    #[test]
    fn an_item_result_without_loot_table_loot_deserializes() {
        // 724 of 70,513 captured item results are exactly this: an empty object.
        let r: DungeonItemResult = serde_json::from_str("{}")
            .expect("retail omits lootTableLoot on an item that generated no loot");
        assert!(r.loot_table_loot.is_empty());
    }

    /// 190 of 68,683 captured enemy results omit `lootTableLoot`.
    #[test]
    fn an_enemy_result_without_loot_table_loot_deserializes() {
        let r: DungeonEnemyResult = serde_json::from_value(serde_json::json!({
            "enemyLevel": 28,
            "givenXP": 106,
            "spawnGroupLoot": {},
        }))
        .expect("retail omits lootTableLoot on an enemy that dropped nothing");
        assert!(r.loot_table_loot.is_empty());
        // merged_loot_table must still work on the defaulted maps, not panic.
        assert!(r.merged_loot_table().stackable_items.is_empty());
    }

    /// The two that were waiting behind the reported one: 347 of 6,564 captured
    /// dungeon bodies omit `chestGeneratedData` and 577 omit `itemGeneratedData`.
    /// Both were required, so both would have produced this same report later.
    #[test]
    fn a_dungeon_body_without_chests_or_items_deserializes() {
        let d: DungeonGeneratedData = serde_json::from_value(serde_json::json!({
            "enemyGeneratedData": {},
            "algorithmVersion": 1,
            "version": 1,
        }))
        .expect("a dungeon with no chests and no ground items is normal retail data");
        assert!(d.item_generated_data.is_empty());
        assert!(d.chest_generated_data.is_empty());
    }

    /// Every collection on the dungeon path omitted at once — the minimal body.
    /// This is the property the fix is for: no single omission can fail an import.
    #[test]
    fn the_minimal_dungeon_body_deserializes() {
        let d: DungeonGeneratedData =
            serde_json::from_str(r#"{"algorithmVersion":1,"version":1}"#)
                .expect("no collection on this path may be required");
        assert!(d.enemy_generated_data.is_empty());
        assert!(d.item_generated_data.is_empty());
        assert!(d.chest_generated_data.is_empty());
    }

    /// Round-trip: an omitted map must not come back as `{}`. Retail never sent the
    /// key, and re-emitting it would change the payload we hand the client.
    #[test]
    fn an_omitted_map_stays_omitted_on_the_way_out() {
        let r: DungeonItemResult = serde_json::from_str("{}").unwrap();
        assert_eq!(serde_json::to_string(&r).unwrap(), "{}");

        let d: DungeonGeneratedData =
            serde_json::from_str(r#"{"algorithmVersion":1,"version":1}"#).unwrap();
        let back = serde_json::to_value(&d).unwrap();
        assert!(back.get("itemGeneratedData").is_none());
        assert!(back.get("chestGeneratedData").is_none());
    }

    /// A populated body must still round-trip — the guard against "default
    /// everything" quietly dropping real loot.
    #[test]
    fn a_populated_body_still_round_trips() {
        let src = serde_json::json!({
            "enemyLevel": 28,
            "givenXP": 106,
            "spawnGroupLoot": {},
            "lootTableLoot": {
                "159bc1e7-454c-4e2a-90cf-e200c74b961a": {
                    "stackableItems": { "159bc1e7-454c-4e2a-90cf-e200c74b961a": 3 }
                }
            },
        });
        let r: DungeonEnemyResult = serde_json::from_value(src).unwrap();
        assert_eq!(r.loot_table_loot.len(), 1);
        let back = serde_json::to_value(&r).unwrap();
        assert!(back.get("lootTableLoot").is_some(), "real loot must survive");
        assert_eq!(r.merged_loot_table().stackable_items.len(), 1);
    }
}
