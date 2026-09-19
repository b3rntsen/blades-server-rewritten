use std::{collections::HashSet, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;
mod wallet;
pub use wallet::{CompleteWallet, WalletEntry};
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
    /// Retail leaves this out, or sends an explicit `null`, whenever the player has
    /// no challenge session running — 773 of 1,032 captured `challengeSeason`
    /// objects omit it.
    ///
    /// It was a bare `Uuid`, which can hold neither. The cost was not an import
    /// failure but something quieter: `arena-transfer.ts` worked around the type by
    /// substituting a hardcoded placeholder session id and default season state for
    /// EVERY transferred character, with the reason written in its own comment —
    /// *"challengeSeason is intentionally left at the default: its captured
    /// currentSessionId is null, which the non-optional struct field can't hold"*.
    /// So every transferred character silently lost its real challenge-season state
    /// because of a type here.
    ///
    /// `Option` + `skip_serializing_if` is the faithful shape, matching `grade` and
    /// `arcane_tier` in `backpack.rs`: retail omits the key rather than sending a
    /// zero, so emitting one would be a shape retail never produced.
    /// THE KEY IS `currentSeasonId`. Retail sent it in **386 of 386** captured
    /// `challengeSeason` objects and never once sent `currentSessionId`, which is
    /// what this emitted.
    ///
    /// The comment above measured "773 of 1,032 omit it" — counting the
    /// misspelled key, which retail does not have, so "omitted" was 100 % true by
    /// construction. The measurement confirmed the typo instead of catching it.
    /// That is the trap in measuring absence: it cannot tell "retail omits this"
    /// from "I asked for the wrong name".
    ///
    /// `alias` keeps the 268 production characters readable — every one of them
    /// has the value stored under the old key, written by the web transfer's
    /// placeholder — so this needs no data migration. Written back under the
    /// correct name on the next save.
    #[serde(
        rename = "currentSeasonId",
        alias = "currentSessionId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub current_season_id: Option<Uuid>,
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
    // When the character last reached a NEW ladder rung, unix seconds. Retail
    // ships it on every character and inside every `pvpSeasonHistory` block; we
    // used to drop it silently on round-trip (no field -> serde discards it), so
    // an imported character lost the timestamp and an archived season block came
    // out one key short of retail's. `default` so existing rows deserialize.
    #[serde(default)]
    pub highest_level_arena_reached_time_secs: i64,
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
                current_season_id: Some(
                    Uuid::from_str("3d336fe7-be60-46a1-b88b-540f3ad5efa2").unwrap(),
                ),
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
            highest_level_arena_reached_time_secs: 0,
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

#[cfg(test)]
mod challenge_season_tests {
    use super::*;

    fn season(extra: &str) -> String {
        format!(
            r#"{{"rank":1,"rankRewarded":0,"points":0,"seasonYear":2026,"premium":false{extra}}}"#
        )
    }

    /// Every assertion below goes through JSON rather than touching the field's
    /// Rust type, so these tests COMPILE against the old bare `Uuid` too and fail
    /// at runtime instead. `quest.rs` learned this the hard way: a compile error is
    /// weaker evidence than watching the deserialize itself fail.
    fn parse(extra: &str) -> Result<serde_json::Value, serde_json::Error> {
        let c: CharacterChallengeSeason = serde_json::from_str(&season(extra))?;
        Ok(serde_json::to_value(&c).unwrap())
    }

    /// THE KEY RETAIL ACTUALLY SENDS.
    ///
    /// Measured over the capture corpus: **386 of 386** `challengeSeason` objects
    /// carry `currentSeasonId`, and not one carries `currentSessionId`.
    ///
    /// The tests this replaces asserted the misspelled key and passed, because
    /// they only ever round-tripped our own output against itself. The comment
    /// above them said retail omitted the key in "773 of 1,032" objects — that
    /// count was of `currentSessionId`, which retail does not have, so it was 100 %
    /// true by construction. Measuring an ABSENCE cannot distinguish "retail omits
    /// this" from "I asked for the wrong name"; only comparing against the key
    /// retail DOES send can.
    #[test]
    fn the_wire_key_is_the_one_retail_sends() {
        let id = "3d336fe7-be60-46a1-b88b-540f3ad5efa2";
        let v = parse(&format!(r#","currentSeasonId":"{id}""#)).unwrap();
        assert_eq!(
            v["currentSeasonId"],
            serde_json::json!(id),
            "retail's key, in 386 of 386 captured objects"
        );
        assert!(
            v.get("currentSessionId").is_none(),
            "and the misspelling must not come back: {v}"
        );
    }

    /// The 268 production characters have the value stored under the OLD key —
    /// the web transfer's placeholder wrote it that way. They must keep it, and
    /// come back out under the correct name, so no data migration is needed.
    #[test]
    fn a_character_stored_under_the_old_key_is_still_read() {
        let id = "3d336fe7-be60-46a1-b88b-540f3ad5efa2";
        let v = parse(&format!(r#","currentSessionId":"{id}""#))
            .expect("the stored shape must still deserialize");
        assert_eq!(
            v["currentSeasonId"],
            serde_json::json!(id),
            "read under the old alias, written back under retail's name"
        );
    }

    /// Absent and explicit-null both still deserialize — a character with no
    /// season must not fail to load. A bare `Uuid` held neither, which is why the
    /// transfer builder substituted a placeholder for every character it imported.
    #[test]
    fn an_absent_or_null_season_id_still_loads() {
        let v = parse("").expect("absent");
        assert!(v.get("currentSeasonId").is_none(), "not invented: {v}");
        let v = parse(r#","currentSeasonId":null"#).expect("explicit null");
        assert!(v.get("currentSeasonId").is_none());
        let v = parse(r#","currentSessionId":null"#).expect("explicit null, old key");
        assert!(v.get("currentSeasonId").is_none());
    }
}
