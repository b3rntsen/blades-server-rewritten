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
    /// MEASURED: the wire key is `items`, plural.
    ///
    /// Across 103,368 captured `lootTableLoot` results retail writes `items`
    /// 9,542 times and `item` **zero** times. We had the singular, which cost
    /// twice over: every item we rolled was published under a key the client
    /// does not read, and every item retail sent us was dropped on import as an
    /// unknown field — silently, because this struct is not
    /// `deny_unknown_fields`.
    ///
    /// The `alias` is for our own stored dungeon rows, which were written under
    /// the old name and are durable. It is not a retail shape.
    #[serde(rename = "items", alias = "item")]
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
    /// Always serialized, even when empty — retail always sends it.
    ///
    /// MEASURED over 66,994 captured enemy results: `spawnGroupLoot` is present
    /// on **every one**, as `{}` in 66,956 of them and filled in 38. It is the
    /// one field here that is never omitted, so skipping it when empty put our
    /// responses in a shape retail never produces.
    ///
    /// It still `default`s on the way in: a field we always send is not
    /// necessarily a field every stored row already has.
    #[serde(default)]
    pub spawn_group_loot: HashMap<Uuid, LootTableResult>,
    /// Omitted when empty — the opposite rule to the field above, and measured
    /// the same way: of those 66,994 results, 190 omit `lootTableLoot` entirely
    /// and **not one** sends it as `{}`. Absent is retail's encoding of "no loot
    /// table" here; `{}` is retail's encoding of it one field up.
    ///
    /// Defaulting on the way in is what report #61 needed: without it those 190
    /// objects fail a whole character import.
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
    /// SIGNED, because retail sent a negative tier.
    ///
    /// One of the 292 chest spawns in the APK carries `rarity: -1`
    /// (`1d8b6737-…`, in `MQ03_DungeonSettings`). That was read as "unset" and
    /// replaced with tier 1. It is not unset: retail put **-1 on the wire**, in
    /// all 5 captured generations of that chest.
    ///
    /// Measured with the control that matters — for the 204 chest spawns whose
    /// APK tier is known AND which retail generated, the tier retail sent
    /// matches the APK exactly, 204 agree and 0 disagree. So the extraction is
    /// right everywhere else, and this one really is -1.
    ///
    /// `u64` could not represent it, which is why the wrong value shipped.
    pub tier: i64,
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

    pub fn get_item(
        &self,
        spawn_group_id: &Uuid,
        spawn_group_index: usize,
    ) -> Option<&DungeonItemResult> {
        self.item_generated_data
            .get(spawn_group_id)
            .and_then(|items| items.get(spawn_group_index))
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
    /// Chests already looted in this run, so a replayed or double-tapped
    /// `chest_collected` cannot mint twice.
    ///
    /// `default` is load-bearing: retail NEVER sends this key -- 0 of 30,771
    /// captured dungeon responses carry it, against 30,158 that carry
    /// `dungeonStatus` -- and every `dungeon_state` row already on the server was
    /// written without it. Without a default, the first read of an existing row
    /// fails to deserialize and the player cannot resume their dungeon.
    ///
    /// Skipped when empty for the same reason: sending a key retail never sent is
    /// how we have broken the client before.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    // Stored as strings so a multi-chest spawn group can be keyed by
    // `uuid-index`. Existing rows used a bare UUID; index zero deliberately
    // keeps that exact spelling, so old in-flight dungeon states deserialize
    // and preserve their already-collected chest.
    pub collected_chests: HashSet<String>,
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

#[cfg(test)]
mod collected_chests_compat {
    use super::*;

    /// A DungeonStatus written before `collected_chests` existed must still load.
    ///
    /// Retail never sends the key (0 of 30,771 captured dungeon responses, against
    /// 30,158 carrying `dungeonStatus`), and every `dungeon_state` row already
    /// stored was written without it. A non-defaulted field here means the first
    /// read of an existing row fails and the player cannot resume their dungeon.
    #[test]
    fn a_status_without_collected_chests_still_deserializes() {
        let raw = r#"{"dungeonSettingsIds":[],"reviveCount":0,"algorithmVersion":1,
            "currentState":{"b64":"AAAA"},"enemyStatus":{},"seed":0,"level":1,"version":1}"#;
        let s: DungeonStatus =
            serde_json::from_str(raw).expect("a pre-existing dungeon status must load");
        assert!(s.collected_chests.is_empty());
    }

    /// Rows written after chest collection was added used a bare UUID. Index zero
    /// deliberately keeps that key, so deploying multi-chest support cannot make
    /// an already-collected chest collectible again.
    #[test]
    fn an_old_bare_uuid_collected_set_still_deserializes() {
        let chest = Uuid::nil().to_string();
        let raw = format!(
            r#"{{"dungeonSettingsIds":[],"reviveCount":0,"algorithmVersion":1,
                "currentState":{{"b64":"AAAA"}},"enemyStatus":{{}},"seed":0,"level":1,
                "version":1,"collectedChests":["{chest}"]}}"#
        );
        let s: DungeonStatus =
            serde_json::from_str(&raw).expect("an indexed build must load the old UUID set");
        assert!(s.collected_chests.contains(&chest));
    }

    /// And we do not hand the client a key retail never sent.
    #[test]
    fn an_empty_collected_set_is_not_serialized() {
        let raw = r#"{"dungeonSettingsIds":[],"reviveCount":0,"algorithmVersion":1,
            "currentState":{"b64":"AAAA"},"enemyStatus":{},"seed":0,"level":1,"version":1}"#;
        let mut s: DungeonStatus = serde_json::from_str(raw).unwrap();
        let out = serde_json::to_value(&s).unwrap();
        assert!(out.get("collectedChests").is_none(), "empty set must be omitted: {out:?}");

        // control: once something IS collected the field appears, so the assertion
        // above is about emptiness and not about the field never existing.
        s.collected_chests.insert(Uuid::nil().to_string());
        let out = serde_json::to_value(&s).unwrap();
        assert!(out.get("collectedChests").is_some(), "a non-empty set must be sent");
    }
}

/// The wire shape of enemy loot, pinned against retail rather than against us.
///
/// The bug these exist for was invisible to a round-trip test: we serialized
/// `item`, deserialized `item`, and agreed with ourselves perfectly while
/// disagreeing with every response retail ever sent. So each test here names a
/// literal retail key and a literal count from the corpus, and none of them
/// compares our output to our own input.
///
/// Corpus: 773 captured `/quests` bodies, 66,994 enemy results inside them,
/// 103,368 `LootTableResult` objects.
#[cfg(test)]
mod enemy_loot_wire_shape {
    use super::*;
    use serde_json::{json, Value};

    fn enemy(json_text: Value) -> DungeonEnemyResult {
        serde_json::from_value(json_text).expect("enemy result parses")
    }

    fn one_item() -> Items {
        let raw = json!([{
            "id": "1c1e4c3c-0000-4000-8000-000000000001",
            "itemTemplateId": "2a2e4c3c-0000-4000-8000-000000000002",
            "grade": 1,
            "durability": 100.0,
            "temperingLevel": 0,
            "properties": {}
        }]);
        serde_json::from_value(raw).expect("item map parses")
    }

    #[test]
    fn loot_items_go_on_the_wire_under_the_plural_key() {
        let result = LootTableResult { item: one_item(), ..Default::default() };
        let wire = serde_json::to_value(&result).unwrap();

        // 9,542 captured results carry `items`. Zero carry `item`.
        assert!(wire.get("items").is_some(), "expected retail's `items`, got {wire}");
        assert!(
            wire.get("item").is_none(),
            "`item` is a key retail never sent; got {wire}"
        );
    }

    #[test]
    fn an_item_retail_sent_us_is_not_dropped_on_the_way_in() {
        // Lifted from a capture: the plural key is what actually arrives.
        let retail = json!({
            "currencies": { "3c3e4c3c-0000-4000-8000-000000000003": 40 },
            "items": [{
                "id": "1c1e4c3c-0000-4000-8000-000000000001",
                "itemTemplateId": "2a2e4c3c-0000-4000-8000-000000000002",
                "grade": 2,
                "durability": 87.5,
                "temperingLevel": 3,
                "properties": {}
            }]
        });
        let parsed: LootTableResult = serde_json::from_value(retail).unwrap();
        assert_eq!(parsed.item.0.len(), 1, "retail's item was silently discarded");
        assert_eq!(parsed.currencies.len(), 1);
    }

    #[test]
    fn a_row_we_stored_under_the_old_singular_key_still_loads() {
        // Not a retail shape — our own durable rows, kept readable by the alias.
        let ours = json!({
            "item": [{
                "id": "1c1e4c3c-0000-4000-8000-000000000001",
                "itemTemplateId": "2a2e4c3c-0000-4000-8000-000000000002",
                "grade": 1,
                "durability": 100.0,
                "temperingLevel": 0,
                "properties": {}
            }]
        });
        let parsed: LootTableResult = serde_json::from_value(ours).unwrap();
        assert_eq!(parsed.item.0.len(), 1, "a stored row stopped loading");
    }

    #[test]
    fn an_empty_loot_result_stays_empty_on_the_wire() {
        // 29,844 captured results are `{}`. None of them spell out empty members,
        // so the three `skip_serializing_if`s are the measured behaviour.
        let wire = serde_json::to_value(LootTableResult::default()).unwrap();
        assert_eq!(wire, json!({}), "an empty result should serialize bare");
    }

    #[test]
    fn spawn_group_loot_is_sent_even_when_it_is_empty() {
        // Present on 66,994 of 66,994 enemy results — empty in 66,956 of them.
        let e = enemy(json!({ "enemyLevel": 12, "givenXP": 340, "lootTableLoot": {} }));
        let wire = serde_json::to_value(&e).unwrap();
        assert_eq!(
            wire.get("spawnGroupLoot"),
            Some(&json!({})),
            "retail never omits spawnGroupLoot; got {wire}"
        );
    }

    #[test]
    fn loot_table_loot_is_omitted_when_it_is_empty() {
        // The opposite rule, and also measured: 190 results omit `lootTableLoot`
        // and none sends `{}`. If this ever starts matching the field above, one
        // of the two rules has been applied to both.
        let e = enemy(json!({ "enemyLevel": 12, "givenXP": 340 }));
        let wire = serde_json::to_value(&e).unwrap();
        assert!(
            wire.get("lootTableLoot").is_none(),
            "empty lootTableLoot should be absent, not `{{}}`; got {wire}"
        );
    }

    #[test]
    fn an_enemy_result_retail_omitted_loot_table_loot_from_still_parses() {
        // The 190. Report #61 was a whole import lost to one of these.
        let e = enemy(json!({ "enemyLevel": 3, "givenXP": 10, "spawnGroupLoot": {} }));
        assert!(e.loot_table_loot.is_empty());
        assert!(e.spawn_group_loot.is_empty());
    }
}
