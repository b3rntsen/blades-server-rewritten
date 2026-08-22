//! Admin / management endpoints.
//!
//! Out-of-band operator endpoints that are **not** part of the Blades client
//! protocol. They're grouped in one module (rather than scattered among the
//! game services) so the management surface is easy to find and lock down:
//! every handler here is dev-token gated, never a game session.
//!
//! Auth: each endpoint requires an `Authorization: Bearer <token>` (or
//! `X-Import-Token: <token>`) header equal to the `ARENA_IMPORT_TOKEN` env var
//! captured at startup. If the env var is unset the admin surface is disabled
//! (503); a missing header is 401; a mismatched one is 403.
//!
//! Endpoints:
//!   * [`import_character`] — `POST /…/api/dev/v1/import-character` — seed a
//!     fully-formed, playable character straight into the `characters` table
//!     (and a backing `users` row if absent), bypassing the create-character
//!     flow. The capture -> server transform lives in the capture platform;
//!     this handler accepts the four `blades_lib` parts (`character`, `data`,
//!     `inventory`, `wallet`) directly.

use std::{collections::HashMap, sync::Arc};

use actix_web::{
    HttpRequest,
    get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::user_data::{
    CompleteCharacter, CompleteCharacterData, CompleteInventory, CompleteWallet,
    DungeonGeneratedData, DungeonGeneratedDataWithId, QuestWithId, UserAccount,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper, insert_into};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    arena::matchmaker::{RecentTicketView, query_recent_matches},
    json_db::JsonDbWrapper,
    models::{CharacterDbAlone, CharacterDbEntry, QuestDbEntry, UserDBEntry},
    schema::{characters, quests, users},
};

// service id used in the BladeApiError envelope for this dev endpoint. Not a
// real Blades service id (those are client-facing); picked to be obviously
// out-of-band so import failures are easy to spot in logs.
const IMPORT_SERVICE_ID: u64 = 9001;

/// The four `blades_lib` parts of a character, as sent on the wire (camelCase).
/// These mirror the JSONB columns of the `characters` table 1:1.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCharacterRequest {
    pub user_id: Uuid,
    pub character: CompleteCharacter,
    pub data: CompleteCharacterData,
    pub inventory: CompleteInventory,
    pub wallet: CompleteWallet,
    /// The character's own captured town (arbitrary JSON), served verbatim by
    /// `get_town`. Optional so older payloads / fresh imports still deserialize;
    /// when absent the existing stored town (if any) is left untouched.
    #[serde(default)]
    pub town: Option<Value>,
    /// The character's captured story quests — the `quests[]` array of a retail
    /// `/quests` response. Without these an imported character arrives with an
    /// empty quest table, so `get_quests` returns `quests: []` and the in-game
    /// quest map has nothing to draw (report #58). Job rows are not sent: those
    /// are rolled server-side per reset window.
    #[serde(default)]
    pub quests: Vec<QuestWithId>,
    /// Dungeon bodies for the quests above, matched by `questId`. A quest whose
    /// dungeon was never generated in the capture simply has no entry here.
    #[serde(default)]
    pub dungeon_generated_data_list: Vec<DungeonGeneratedDataWithId>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCharacterResponse {
    pub character_id: Uuid,
    pub user_id: Uuid,
    /// true if a brand-new character row was inserted; false if an existing
    /// row for this `userId` was overwritten.
    pub created: bool,
}

/// Pull the dev token out of the request, checking `Authorization: Bearer ...`
/// first and falling back to `X-Import-Token`.
fn extract_import_token(req: &HttpRequest) -> Option<String> {
    if let Some(value) = req.headers().get("Authorization") {
        if let Ok(value) = value.to_str() {
            if let Some(token) = value.strip_prefix("Bearer ") {
                return Some(token.trim().to_string());
            }
        }
    }
    if let Some(value) = req.headers().get("X-Import-Token") {
        if let Ok(value) = value.to_str() {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Validate the dev token against the one captured at startup. Returns the
/// appropriate `BladeApiError` on failure so the caller can `?` it.
fn check_import_token(app_state: &ServerGlobal, req: &HttpRequest) -> Result<(), BladeApiError> {
    // No token configured -> endpoint disabled.
    let expected = match app_state.arena_import_token.as_deref() {
        Some(token) if !token.is_empty() => token,
        _ => return Err(BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, IMPORT_SERVICE_ID, 1)),
    };

    match extract_import_token(req) {
        // constant-time-ish: lengths differ -> mismatch; otherwise compare.
        Some(provided) if provided == expected => Ok(()),
        Some(_) => Err(BladeApiError::new(StatusCode::FORBIDDEN, IMPORT_SERVICE_ID, 2)),
        None => Err(BladeApiError::new(StatusCode::UNAUTHORIZED, IMPORT_SERVICE_ID, 3)),
    }
}

#[post("/blades.bgs.services/api/dev/v1/import-character")]
pub async fn import_character(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<ImportCharacterRequest>,
) -> Result<Json<ImportCharacterResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;

    let body = body.into_inner();
    let user_id = body.user_id;

    let mut conn = app_state.db_pool.get().await.unwrap();

    let response = conn
        .transaction::<_, BladeApiError, _>(|mut conn| {
            async move {
                // 1. Ensure a backing `users` row exists (characters.user_id is a
                //    NOT NULL FK -> users.id). If absent, insert a minimal user:
                //    a random secret_id and an empty UserAccount (no device ids).
                //    We never overwrite an existing user row here.
                let existing_user: i64 = users::table
                    .filter(users::id.eq(user_id))
                    .count()
                    .get_result(&mut conn)
                    .await?;

                if existing_user == 0 {
                    insert_into(users::table)
                        .values(UserDBEntry {
                            id: user_id,
                            secret_id: Uuid::new_v4(),
                            data: JsonDbWrapper(UserAccount::new_random()),
                        })
                        .execute(&mut conn)
                        .await?;
                }

                // 2. Upsert the character row. `characters` has a UNIQUE(user_id)
                //    constraint (one char per user), so look up the existing row
                //    (locking it) and either overwrite its four JSONB columns or
                //    insert a fresh row with a new id.
                let existing: Option<CharacterDbAlone> = characters::table
                    .filter(characters::user_id.eq(user_id))
                    .select(CharacterDbAlone::as_select())
                    .for_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next();

                let (character_id, created) = match existing {
                    Some(row) => (row.id, false),
                    None => (Uuid::new_v4(), true),
                };

                let entry = CharacterDbEntry {
                    id: character_id,
                    user_id,
                    character: JsonDbWrapper(body.character),
                    data: JsonDbWrapper(body.data),
                    wallet: JsonDbWrapper(body.wallet),
                    inventory: JsonDbWrapper(body.inventory),
                    town: body.town.map(JsonDbWrapper),
                };

                if created {
                    insert_into(characters::table)
                        .values(&entry)
                        .execute(&mut conn)
                        .await?;
                } else {
                    // Overwrite all four payload columns of the existing row.
                    diesel::update(characters::table)
                        .filter(characters::id.eq(character_id))
                        .set((
                            characters::character.eq(entry.character),
                            characters::data.eq(entry.data),
                            characters::wallet.eq(entry.wallet),
                            characters::inventory.eq(entry.inventory),
                        ))
                        .execute(&mut conn)
                        .await?;
                    // Town is overwritten only when the payload carries one, so a
                    // re-import without a captured town doesn't wipe a good one.
                    if let Some(town) = entry.town {
                        diesel::update(characters::table)
                            .filter(characters::id.eq(character_id))
                            .set(characters::town.eq(town))
                            .execute(&mut conn)
                            .await?;
                    }
                }

                // 3. Seed the character's captured story quests. Before this an
                //    imported character had no `quests` rows at all, so
                //    `get_quests` returned `quests: []` and the quest map was
                //    empty for every transferred player (report #58).
                //
                //    `do_nothing` on conflict: a re-import restores what the
                //    capture holds without clobbering progress the player has
                //    made on the live server since. Same reasoning as `town`
                //    above, and the same conflict target the job upsert in
                //    `quest.rs` uses.
                if !body.quests.is_empty() {
                    let mut dungeons: HashMap<Uuid, DungeonGeneratedData> = body
                        .dungeon_generated_data_list
                        .into_iter()
                        .map(|d| (d.quest_id, d.inner))
                        .collect();

                    for quest in body.quests {
                        let entry = QuestDbEntry {
                            id: quest.quest_id,
                            character_id,
                            info: JsonDbWrapper(quest.quest),
                            generated_data: JsonDbWrapper(dungeons.remove(&quest.quest_id)),
                            dungeon_state: None,
                        };
                        insert_into(quests::table)
                            .values(&entry)
                            .on_conflict((quests::id, quests::character_id))
                            .do_nothing()
                            .execute(&mut conn)
                            .await?;
                    }
                }

                Ok(ImportCharacterResponse {
                    character_id,
                    user_id,
                    created,
                })
            }
            .scope_boxed()
        })
        .await?;

    Ok(Json(response))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentMatchesQuery {
    #[serde(default)]
    user_id: Option<Uuid>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /…/api/dev/v1/recent-matches?userId=<uuid>&limit=<n>` — the most recent
/// matchmaking tickets (newest first), so the web /arena page can confirm a
/// user's match request registered + show recent arena activity. Dev-token
/// gated. `userId` only sets the per-row `mine` flag (the list is server-wide).
/// Durable: backed by the `arena_matches` table, so it survives restarts (#NB-3).
#[get("/blades.bgs.services/api/dev/v1/recent-matches")]
pub async fn recent_matches(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<RecentMatchesQuery>,
) -> Result<Json<Vec<RecentTicketView>>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let q = query.into_inner();
    let limit = q.limit.unwrap_or(25).min(100);
    Ok(Json(
        query_recent_matches(&app_state.db_pool, limit as i64, q.user_id).await,
    ))
}

// ---------------------------------------------------------------------------
// Per-player claim link: bind a device's anon `deviceId` to a Transfer'd
// character's user, and list recently-seen devices for the web claim UI. Both
// dev-token gated (same as import). The device_bindings table + the anon_log_in
// lookup are in migration 2026-06-08_add_device_bindings / authentification.rs.
// Raw SQL (sql_query) avoids a timestamp-typed diesel schema.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindDeviceRequest {
    pub device_id: String,
    pub user_id: Uuid,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindDeviceResponse {
    pub device_id: String,
    pub user_id: Uuid,
}

/// `POST /…/api/dev/v1/bind-device` — bind a device to a user (the per-player
/// claim link); after this the device's `auth/anon` logs in as that user. Upsert
/// by device_id, so re-claiming moves the binding.
#[post("/blades.bgs.services/api/dev/v1/bind-device")]
pub async fn bind_device(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<BindDeviceRequest>,
) -> Result<Json<BindDeviceResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let body = body.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();
    diesel::sql_query(
        "INSERT INTO device_bindings (device_id, user_id, bound_at, last_seen) \
         VALUES ($1, $2, now(), now()) \
         ON CONFLICT (device_id) DO UPDATE SET user_id = $2, bound_at = now()",
    )
    .bind::<diesel::sql_types::Text, _>(body.device_id.clone())
    .bind::<diesel::sql_types::Uuid, _>(body.user_id)
    .execute(&mut conn)
    .await
    .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 10))?;
    Ok(Json(BindDeviceResponse {
        device_id: body.device_id,
        user_id: body.user_id,
    }))
}

/// One recently-seen device, for the claim UI (most-recent first).
#[derive(Serialize, diesel::QueryableByName)]
#[serde(rename_all = "camelCase")]
pub struct RecentDevice {
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub device_id: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    pub user_id: Option<Uuid>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    pub platform: Option<String>,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub age_seconds: i64,
}

/// `GET /…/api/dev/v1/recent-devices` — list recently-seen devices so a player
/// can pick the one they just launched and claim it.
#[get("/blades.bgs.services/api/dev/v1/recent-devices")]
pub async fn recent_devices(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<Vec<RecentDevice>>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let mut conn = app_state.db_pool.get().await.unwrap();
    let rows = diesel::sql_query(
        "SELECT device_id, user_id, platform, \
         CAST(EXTRACT(epoch FROM (now() - last_seen)) AS BIGINT) AS age_seconds \
         FROM device_bindings ORDER BY last_seen DESC LIMIT 50",
    )
    .get_results::<RecentDevice>(&mut conn)
    .await
    .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 11))?;
    Ok(Json(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use blades_lib::user_data::{
        CompleteCharacter, CompleteCharacterData, CompleteInventory, CompleteWallet,
    };

    /// The wire contract: a representative camelCase JSON body deserializes
    /// into `ImportCharacterRequest` with the four `blades_lib` parts intact.
    ///
    /// We build the body by serializing the library defaults (so the test
    /// stays in lockstep with the real serde shapes of each part) and only
    /// hand-write the `userId` and a couple of character fields we then assert.
    #[test]
    fn import_request_deserializes_from_wire_json() {
        let user_id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();

        let mut character = CompleteCharacter::default();
        character.name = "DevSeed".to_string();
        character.level = 42;

        let body = serde_json::json!({
            "userId": user_id,
            "character": character,
            "data": CompleteCharacterData::default(),
            "inventory": {
                "backpack": serde_json::to_value(blades_lib::user_data::Backpack::default()).unwrap(),
                "loadout": serde_json::to_value(blades_lib::user_data::Loadout::default()).unwrap(),
                "treasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "overflowTreasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "backpackVersion": 1,
                "treasuryVersion": 0,
            },
            "wallet": CompleteWallet::default(),
        });

        let parsed: ImportCharacterRequest =
            serde_json::from_value(body).expect("representative import body must deserialize");

        assert_eq!(parsed.user_id, user_id);
        assert_eq!(parsed.character.name, "DevSeed");
        assert_eq!(parsed.character.level, 42);
        // sanity: the other parts round-tripped into their concrete types.
        assert_eq!(parsed.inventory.backpack_version, 1);
        let _: CompleteInventory = parsed.inventory;
        let _: CompleteWallet = parsed.wallet;
        let _: CompleteCharacterData = parsed.data;
    }

    /// `new-flags` is renamed (dash, not camelCase). Make sure a body using the
    /// real key deserializes and round-trips through `CompleteCharacterData`.
    #[test]
    fn data_part_accepts_new_flags_dash_key() {
        let body = serde_json::json!({
            "userId": "11111111-2222-3333-4444-555555555555",
            "character": CompleteCharacter::default(),
            "data": {
                "customization": { "CharacterUID": "abc" },
                "new-flags": { "seen_intro": true },
                "dialog": {}
            },
            "inventory": {
                "backpack": serde_json::to_value(blades_lib::user_data::Backpack::default()).unwrap(),
                "loadout": serde_json::to_value(blades_lib::user_data::Loadout::default()).unwrap(),
                "treasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "overflowTreasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "backpackVersion": 1,
                "treasuryVersion": 0,
            },
            "wallet": CompleteWallet::default(),
        });

        let parsed: ImportCharacterRequest =
            serde_json::from_value(body).expect("body with new-flags must deserialize");
        assert_eq!(parsed.data.new_flags["seen_intro"], serde_json::json!(true));
    }

    /// The inert parts of an import body, so the quest tests below show only
    /// what they are about.
    fn body_without_quests() -> serde_json::Value {
        serde_json::json!({
            "userId": "11111111-2222-3333-4444-555555555555",
            "character": CompleteCharacter::default(),
            "data": CompleteCharacterData::default(),
            "inventory": {
                "backpack": serde_json::to_value(blades_lib::user_data::Backpack::default()).unwrap(),
                "loadout": serde_json::to_value(blades_lib::user_data::Loadout::default()).unwrap(),
                "treasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "overflowTreasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "backpackVersion": 1,
                "treasuryVersion": 0,
            },
            "wallet": CompleteWallet::default(),
        })
    }

    /// Report #58: transferred characters arrived with an empty `quests` table,
    /// so `get_quests` returned `quests: []` and the in-game quest map was
    /// blank. The import body must carry the captured story quests.
    ///
    /// The fixture is a verbatim entry from a real pre-shutdown capture of
    /// `GET /characters/{id}/quests` (capture 462), not an invented shape —
    /// including the `difficultyLevel: -1` that marks a story quest and the
    /// `questId == gldQuestId` identity retail used for them.
    #[test]
    fn import_request_carries_captured_story_quests() {
        let quest_id = "159bc1e7-454c-4e2a-90cf-e200c74b961a";
        let objective_id = "64b2ac8f-9500-4101-b25b-87b41df1b6d7";

        let mut body = body_without_quests();
        body["quests"] = serde_json::json!([{
            "questId": quest_id,
            "version": 2,
            "type": "NORMAL",
            "objectiveStatuses": {
                objective_id: { "status": "Active", "progress": 0.0, "completed": false }
            },
            "difficultyLevel": -1,
            "seed": 485975867u64,
            "gldQuestId": quest_id,
            "completed": false,
        }]);

        let parsed: ImportCharacterRequest =
            serde_json::from_value(body).expect("a captured quests array must deserialize");

        assert_eq!(parsed.quests.len(), 1, "the captured quest must survive");
        let q = &parsed.quests[0];
        assert_eq!(q.quest_id, Uuid::parse_str(quest_id).unwrap());
        assert_eq!(q.quest.gld_quest_id, Uuid::parse_str(quest_id).unwrap());
        assert_eq!(q.quest.seed, 485975867);
        // -1 is what retail sends for story quests; jobs carry a real difficulty.
        assert_eq!(q.quest.difficulty_level, -1);
        assert!(!q.quest.completed);
        assert_eq!(
            q.quest.objective_statuses.len(),
            1,
            "objective progress must not be dropped — it is what the map draws"
        );
    }

    /// Older capture-side callers send no `quests` key at all. They must keep
    /// working and simply seed nothing, rather than failing the whole import.
    #[test]
    fn import_request_without_quests_still_deserializes() {
        let parsed: ImportCharacterRequest = serde_json::from_value(body_without_quests())
            .expect("a body predating the quests field must still deserialize");
        assert!(parsed.quests.is_empty());
        assert!(parsed.dungeon_generated_data_list.is_empty());
    }
}
