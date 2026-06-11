use std::{collections::HashSet, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;
mod wallet;
pub use wallet::CompleteWallet;
mod backpack;
pub use backpack::*;
mod dungeon;
pub use dungeon::*;
mod quest;
pub use quest::*;
mod util;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CompleteCharacterData {
    pub customization: Value,
    #[serde(rename = "new-flags")]
    #[serde(default)]
    pub new_flags: Value,
    #[serde(default)]
    pub dialog: Value,
}

impl Default for CompleteCharacterData {
    fn default() -> Self {
        CompleteCharacterData {
            customization: json!({}),
            new_flags: json!({}),
            dialog: json!({}),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CharacterChallengeSeason {
    pub current_session_id: Uuid,
    pub rank: i64,
    pub rank_rewarded: i64,
    pub points: i64,
    pub season_year: u64,
    pub premium: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
// May also be sent to the user on initial sync (does not have the id field, see #[serde(flatten)])
pub struct CompleteCharacter {
    pub name: String,
    pub tag_id: String,
    // Town-RPG progression sub-objects. Not modeled when the server was
    // arena-only (the in-match loadout flows from the client at
    // PlayerLoadoutReady), but the full-game menu/town load validates them — a
    // leveled character with these missing is rejected and the client hangs. We
    // carry them verbatim from the captured character (serde_json::Value), stored
    // in the existing `character` JSONB. Omitted when null so a fresh character's
    // wire is unchanged (it never carried them).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub equipped_abilities: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub abilities: Value,
    pub version: u64,
    pub level: u16,
    pub experience: u64,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub completed_quests: Value,
    pub maximum_abyss_level_reached: u16,

    // Was Option<()> (always null); widened to Value so an in-progress town
    // dungeon round-trips. Defaults to null and is always emitted (unchanged wire).
    #[serde(default)]
    pub current_quest_dungeon: Value,
    pub last_jobs_reset_time: u64,
    pub inventory_level: u16,
    pub stamina_attribute_points: u32,
    pub magicka_attribute_points: u32,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub global_shop_offers: Value,
    pub challenge_season: CharacterChallengeSeason,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub loadout_profiles: Value,
    pub last_guild_exchange_request_time: u64,
    pub last_guild_exchange_donation_time: u64,
    pub guild_exchange_donation_count: i64,
    pub pvp_chest_meter: i64,
    pub pvp_winning_streak: i64,
    pub pvp_exception_easier_match_remaining: i64,
    pub pvp_exception_harder_match_remaining: i64,
    pub matchmaking_pvp_trophies: i64,
    pub pvp_trophies: i64,
    pub highest_arena_reached: u64,
    pub highest_level_arena_reached: u64,
    pub number_pvp_match_played: i64,
    pub trophy_count_modifier: i64,
    pub pvp_season_id: Uuid,
    // The arena / full-game flow validates the character's PvP-season state. A
    // transferred char must carry its real season history; the import used to
    // drop it (the struct had no field for it) -> an incoherent season. Carried
    // verbatim like the progression sub-objects; omitted when null (fresh char).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub pvp_season_history: Value,
    pub job_difficulty_cycle_index: i64,
    pub validation_flags: u32,
    pub treasury_level: u32,
    pub name_validated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub avatar_icon_id: Option<Uuid>,
}

impl Default for CompleteCharacter {
    fn default() -> Self {
        CompleteCharacter {
            name: String::default(),
            tag_id: "1234".to_string(),
            version: 1,
            level: 1,
            experience: 1,
            maximum_abyss_level_reached: 0,
            current_quest_dungeon: Value::Null,
            equipped_abilities: Value::Null,
            abilities: Value::Null,
            completed_quests: Value::Null,
            global_shop_offers: Value::Null,
            loadout_profiles: Value::Null,
            last_jobs_reset_time: 0,
            inventory_level: 0,
            stamina_attribute_points: 0,
            magicka_attribute_points: 0,
            challenge_season: CharacterChallengeSeason {
                current_session_id: Uuid::from_str("3d336fe7-be60-46a1-b88b-540f3ad5efa2").unwrap(),
                rank: 1,
                rank_rewarded: 0,
                points: 0,
                season_year: 2026,
                premium: false,
            },
            last_guild_exchange_request_time: 0,
            last_guild_exchange_donation_time: 0,
            guild_exchange_donation_count: 0,
            pvp_chest_meter: 0,
            pvp_winning_streak: 0,
            pvp_exception_easier_match_remaining: 0,
            pvp_exception_harder_match_remaining: 0,
            matchmaking_pvp_trophies: 0,
            pvp_trophies: 0,
            highest_arena_reached: 1,
            highest_level_arena_reached: 1,
            number_pvp_match_played: 0,
            trophy_count_modifier: 0,
            pvp_season_id: Uuid::default(),
            pvp_season_history: Value::Null,
            job_difficulty_cycle_index: 0,
            validation_flags: 1,
            treasury_level: 0,
            name_validated: true,
            avatar_icon_id: None,
        }
    }
}

#[derive(Serialize, Debug)]
pub struct CompleteCharacterWithIdWithoutData {
    pub id: Uuid,
    #[serde(flatten)]
    pub character: CompleteCharacter,
}

#[derive(Serialize)]
pub struct CompleteCharacterWithIdAndData {
    pub data: CompleteCharacterData,
    pub id: Uuid,
    #[serde(flatten)]
    pub character: CompleteCharacter,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct UserAccount {
    pub gp_deviceids: HashSet<String>,
}

impl UserAccount {
    pub fn new_random() -> Self {
        UserAccount {
            gp_deviceids: HashSet::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct B64EncodedData {
    pub b64: String,
}
