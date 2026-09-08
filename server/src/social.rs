//! `GET /social/characters` — resolve a set of user ids to their public character
//! cards.
//!
//! Every screen that shows *other* players goes through here: the guild roster,
//! the guild message log, friends, arena opponent cards. The client holds only
//! `userId`s (a `guilds/current` member object carries exactly four keys —
//! `userId`, `guildId`, `rank`, `joinDate`, confirmed against all 20 members of a
//! captured retail response), so a name, level or portrait can only come from
//! this endpoint.
//!
//! We never implemented it. Retail answered **200 on 865 of 865** captured calls;
//! our own server answered **404 on 3 of 3**. The visible symptom is a guild
//! screen that spins forever with nothing in the log: `guilds/current` returns
//! 200, the client then asks who the members are, gets a 404, and stops — it
//! never issues the `guilds/current/messages` and `guilds/current/exchanges`
//! calls that retail fires immediately afterwards.
//!
//! Three query shapes appear in the captures:
//!
//! ```text
//! ?userIds=<id>[,<id>...]&format=short
//! ?userIds=<id>&format=full&characterDataKeys=customization
//! ?userIds=<id>&format=full&characterDataKeys=dialog
//! ```
//!
//! `format=short` is the roster card: seven keys, present on 77 of 77 characters
//! across a 12-body sample. `format=full` adds the progression fields and the
//! requested slices of `data` — it is what the arena appearance path asks for,
//! and all three of our observed 404s were `characterDataKeys=customization`.

use std::sync::Arc;

use actix_web::{
    get,
    http::StatusCode,
    web::{self, Json},
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, models::CharacterDbEntryCharacterAndData, schema,
    session::SessionLookedUpMaybe,
};

const SOCIAL_SERVICE_ID: u64 = 9006;

/// A ceiling on how many ids one call may resolve, so a malformed or hostile
/// `userIds` cannot turn into an unbounded `IN (...)`. The largest captured
/// request asks for a handful; a full guild roster is capped at 50 members by
/// retail's own guild rules, and the arena asks about one opponent at a time.
const MAX_USER_IDS: usize = 200;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocialQuery {
    /// Comma-separated. Sent percent-encoded (`%2C`) by the client; actix decodes
    /// it before serde sees it.
    user_ids: Option<String>,
    format: Option<String>,
    /// Which `data` slices `format=full` should carry — `customization`, `dialog`.
    /// Comma-separated, same as `userIds`.
    character_data_keys: Option<String>,
}

/// `presence.showPlayerAs` — retail sends the string `"online"` or `"offline"`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PresenceWire {
    /// Milliseconds, not seconds. Retail pairs `offline` with `0` on 43 of 77
    /// sampled characters, so a player we have no activity record for gets that
    /// exact shape rather than an invented timestamp. `online` was never paired
    /// with `0` in the sample, so a live session reports "now" — which for a
    /// player holding an unexpired session is what it says.
    last_activity_date: i64,
    show_player_as: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialCharacterWire {
    user_id: Uuid,
    character_id: Uuid,
    level: u16,
    pvp_trophies: i64,
    /// Retail sent this on 77 of 77 sampled characters, so it is never omitted.
    /// A character who has not chosen a portrait has `None` in the DB; we send
    /// the nil UUID, which is the "no icon" value the client already handles
    /// (`avatar_icon_id` is `None` on every freshly created character).
    avatar_icon_id: Uuid,
    presence: PresenceWire,
    name: String,

    // ── format=full only ────────────────────────────────────────────────────
    #[serde(skip_serializing_if = "Option::is_none")]
    experience: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    guild_exchange_donation_count: Option<i64>,
    #[serde(skip_serializing_if = "Value::is_null")]
    completed_quests: Value,
    #[serde(skip_serializing_if = "Value::is_null")]
    equipped_abilities: Value,
    /// Only the `characterDataKeys` that were asked for. Omitted entirely for
    /// `format=short`, which never carried it.
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl SocialCharacterWire {
    /// Build one card. Split out of the handler so the wire shape can be pinned
    /// against the captured retail bodies without a database.
    #[allow(clippy::too_many_arguments)]
    fn build(
        user_id: Uuid,
        character_id: Uuid,
        c: &blades_lib::user_data::CompleteCharacter,
        data: &blades_lib::user_data::CompleteCharacterData,
        is_online: bool,
        now_ms: i64,
        full: bool,
        data_keys: &[String],
    ) -> Self {
        SocialCharacterWire {
            user_id,
            character_id,
            level: c.level,
            pvp_trophies: c.pvp_trophies,
            avatar_icon_id: c.avatar_icon_id.unwrap_or(Uuid::nil()),
            presence: PresenceWire {
                last_activity_date: if is_online { now_ms } else { 0 },
                show_player_as: if is_online { "online" } else { "offline" },
            },
            name: c.name.clone(),
            experience: full.then_some(c.experience),
            guild_exchange_donation_count: full.then_some(c.guild_exchange_donation_count),
            completed_quests: if full {
                c.completed_quests.clone()
            } else {
                Value::Null
            },
            equipped_abilities: if full {
                c.equipped_abilities.clone()
            } else {
                Value::Null
            },
            data: (full && !data_keys.is_empty()).then(|| select_data_keys(data, data_keys)),
        }
    }
}

#[derive(Serialize)]
struct SocialCharacters {
    characters: Vec<SocialCharacterWire>,
}

/// `{"social":{"characters":[...]}}` — the wrapper is two levels deep in every
/// captured response, including the ones that resolve a single id.
#[derive(Serialize)]
pub struct SocialResponse {
    social: SocialCharacters,
}

/// Split a comma-separated query value, dropping empties so a trailing comma or
/// a doubled separator is not read as a blank entry.
fn split_csv(raw: &str) -> impl Iterator<Item = &str> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty())
}

#[get("/blades.bgs.services/api/game/v1/public/social/characters")]
pub async fn get_social_characters(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<SocialQuery>,
) -> Result<Json<SocialResponse>, BladeApiError> {
    // Any authenticated player may ask about any other: this is the same
    // visibility retail granted, and it is what makes a guild roster or an
    // opponent card render at all.
    session.get_session_or_error()?;
    let q = query.into_inner();

    let raw_ids = q.user_ids.unwrap_or_default();
    // An unparseable id is skipped rather than fatal: retail returns the
    // characters it could resolve, and one bad entry must not blank a whole
    // roster. A wholly unresolvable list therefore yields `characters: []`.
    let mut wanted: Vec<Uuid> = split_csv(&raw_ids)
        .filter_map(|s| Uuid::parse_str(s).ok())
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    if wanted.len() > MAX_USER_IDS {
        return Err(BladeApiError::new(
            StatusCode::BAD_REQUEST,
            SOCIAL_SERVICE_ID,
            10,
        ));
    }
    if wanted.is_empty() {
        return Ok(Json(SocialResponse {
            social: SocialCharacters {
                characters: Vec::new(),
            },
        }));
    }

    let full = q.format.as_deref() == Some("full");
    let data_keys: Vec<String> = q
        .character_data_keys
        .as_deref()
        .map(|raw| split_csv(raw).map(str::to_owned).collect())
        .unwrap_or_default();

    let mut conn = app_state.db_pool.get().await.unwrap();

    let rows: Vec<CharacterDbEntryCharacterAndData> = {
        use schema::characters::dsl::*;
        characters
            .filter(user_id.eq_any(&wanted))
            .select(CharacterDbEntryCharacterAndData::as_select())
            .load(&mut conn)
            .await?
    };

    // One grouped query for presence rather than a lookup per character: a live
    // (unexpired) session row is the only online signal we have.
    let online: Vec<Uuid> = {
        use schema::sessions::dsl::*;
        sessions
            .filter(user_id.eq_any(&wanted))
            .filter(expires_at.gt(diesel::dsl::now))
            .select(user_id)
            .load(&mut conn)
            .await?
    };

    let now_ms = crate::arena::arena_season::now_unix() * 1000;

    let characters = rows
        .iter()
        .map(|row| {
            SocialCharacterWire::build(
                row.user_id,
                row.id,
                &row.character.0,
                &row.data.0,
                online.contains(&row.user_id),
                now_ms,
                full,
                &data_keys,
            )
        })
        .collect();

    Ok(Json(SocialResponse {
        social: SocialCharacters { characters },
    }))
}

/// Project a character's `data` down to the requested `characterDataKeys`.
///
/// Retail's `format=full` response carried only the slice that was asked for —
/// a `characterDataKeys=customization` call comes back with `data.customization`
/// and nothing else — so returning the whole blob would send keys retail never
/// sent on this route. A key the character has no value for is omitted rather
/// than sent as null, matching how the character's own `data` is serialized.
fn select_data_keys(data: &blades_lib::user_data::CompleteCharacterData, keys: &[String]) -> Value {
    // Round-trip through the character's own serializer so the key names and
    // shapes here cannot drift from the ones `/characters/{id}` emits.
    let all = serde_json::to_value(data).unwrap_or(Value::Null);
    let mut out = Map::new();
    if let Value::Object(map) = all {
        for k in keys {
            if let Some(v) = map.get(k) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// The `/social/characters` wire, against the captured retail bodies.
///
/// The sample: 12 `format=short` responses holding 77 characters, plus the
/// smallest `format=full&characterDataKeys=customization` body.
#[cfg(test)]
mod wire {
    use super::*;
    use blades_lib::user_data::{CompleteCharacter, CompleteCharacterData};
    use serde_json::json;

    fn character() -> CompleteCharacter {
        CompleteCharacter {
            name: "Gretchen the Formidable".into(),
            level: 94,
            pvp_trophies: 565,
            experience: 4_210_000,
            guild_exchange_donation_count: 3,
            completed_quests: json!(["a8d46714-ebd9-4e78-8265-770622dabfef"]),
            equipped_abilities: json!({"0": "heal"}),
            avatar_icon_id: Some(Uuid::from_u128(0xe5ca_7779)),
            ..CompleteCharacter::default()
        }
    }

    fn data() -> CompleteCharacterData {
        CompleteCharacterData {
            customization: json!({"TagId": {"_t": "String", "_v": "1656"}}),
            dialog: json!({"seen": 4}),
            ..CompleteCharacterData::default()
        }
    }

    fn build(full: bool, keys: &[&str]) -> Value {
        let keys: Vec<String> = keys.iter().map(|s| s.to_string()).collect();
        serde_json::to_value(SocialCharacterWire::build(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            &character(),
            &data(),
            false,
            1_778_434_827_424,
            full,
            &keys,
        ))
        .unwrap()
    }

    /// `format=short` is exactly seven keys — all seven present on 77 of 77
    /// sampled characters, and no eighth ever appearing.
    #[test]
    fn short_is_the_seven_keys_retail_sent() {
        let v = build(false, &[]);
        let obj = v.as_object().expect("character object");
        let mut got: Vec<&str> = obj.keys().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(
            got,
            [
                "avatarIconId",
                "characterId",
                "level",
                "name",
                "presence",
                "pvpTrophies",
                "userId"
            ],
            "format=short must carry exactly retail's seven keys"
        );
    }

    /// The progression fields belong to `format=full` only. Adding a key to a
    /// route retail never sent it on is its own bug — that is what the
    /// stub-character load stall was.
    #[test]
    fn full_adds_the_progression_fields_and_short_does_not() {
        let extra = [
            "experience",
            "guildExchangeDonationCount",
            "completedQuests",
            "equippedAbilities",
            "data",
        ];

        let short = build(false, &["customization"]);
        for key in extra {
            assert!(
                short.get(key).is_none(),
                "format=short must not carry {key}, got {short:?}"
            );
        }

        let full = build(true, &["customization"]);
        for key in extra {
            assert!(
                full.get(key).is_some(),
                "format=full must carry {key}, got {:?}",
                full.as_object().map(|o| o.keys().collect::<Vec<_>>())
            );
        }
    }

    /// `characterDataKeys` is a projection, not a hint: a `customization` call
    /// gets `customization` and nothing else. Without this the response would
    /// carry `dialog` and `new-flags` on a route that never had them.
    #[test]
    fn data_carries_only_the_requested_keys() {
        let v = build(true, &["customization"]);
        let d = v.get("data").and_then(Value::as_object).expect("data");
        assert!(d.contains_key("customization"));
        assert_eq!(
            d.len(),
            1,
            "only the requested key belongs in data, got {:?}",
            d.keys().collect::<Vec<_>>()
        );

        // control: asking for the other key gets the other key, so the
        // assertion above is a projection and not a hardcoded single field.
        let v = build(true, &["dialog"]);
        let d = v.get("data").and_then(Value::as_object).expect("data");
        assert!(
            d.contains_key("dialog"),
            "got {:?}",
            d.keys().collect::<Vec<_>>()
        );
        assert!(!d.contains_key("customization"));
    }

    /// Retail paired `offline` with `lastActivityDate: 0` on 43 of 77 sampled
    /// characters, and never paired `online` with `0`.
    #[test]
    fn presence_pairs_the_way_retail_paired_it() {
        let offline = build(false, &[]);
        let p = offline.get("presence").expect("presence");
        assert_eq!(p.get("showPlayerAs").unwrap(), "offline");
        assert_eq!(p.get("lastActivityDate").unwrap(), 0);

        let v = serde_json::to_value(SocialCharacterWire::build(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            &character(),
            &data(),
            true,
            1_778_434_827_424,
            false,
            &[],
        ))
        .unwrap();
        let p = v.get("presence").expect("presence");
        assert_eq!(p.get("showPlayerAs").unwrap(), "online");
        assert_eq!(
            p.get("lastActivityDate").unwrap(),
            1_778_434_827_424i64,
            "an online player must not report 0: retail never sent that pair"
        );
    }

    /// A character who never picked a portrait still gets the key: retail sent
    /// `avatarIconId` on 77 of 77, so omitting it is a shape it never produced.
    #[test]
    fn avatar_icon_is_always_present() {
        let c = CompleteCharacter {
            avatar_icon_id: None,
            ..character()
        };
        let v = serde_json::to_value(SocialCharacterWire::build(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            &c,
            &data(),
            false,
            0,
            false,
            &[],
        ))
        .unwrap();
        assert_eq!(v.get("avatarIconId").unwrap(), &json!(Uuid::nil()));
    }

    /// The two-level `{"social":{"characters":[...]}}` wrapper, present on every
    /// captured response including single-id ones.
    #[test]
    fn the_response_is_wrapped_twice() {
        let r = SocialResponse {
            social: SocialCharacters { characters: vec![] },
        };
        let v = serde_json::to_value(&r).unwrap();
        assert!(
            v.get("social").and_then(|s| s.get("characters")).is_some(),
            "expected social.characters, got {v:?}"
        );
    }

    /// A trailing or doubled comma must not become a blank id, and an
    /// unparseable entry must not blank the whole roster.
    #[test]
    fn csv_parsing_survives_junk() {
        let ids: Vec<&str> = split_csv("a, ,b,,c,").collect();
        assert_eq!(ids, ["a", "b", "c"]);

        let good = Uuid::from_u128(7).to_string();
        let parsed: Vec<Uuid> = split_csv(&format!("not-a-uuid,{good}"))
            .filter_map(|s| Uuid::parse_str(s).ok())
            .collect();
        assert_eq!(
            parsed,
            [Uuid::from_u128(7)],
            "one bad id must not drop the good one"
        );
    }
}
