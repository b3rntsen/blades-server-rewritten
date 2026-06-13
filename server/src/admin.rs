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

use std::sync::Arc;

use actix_web::{
    HttpRequest,
    get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::user_data::{
    CompleteCharacter, CompleteCharacterData, CompleteInventory, CompleteWallet, UserAccount,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper, insert_into};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    arena::matchmaker::RecentTicketView,
    json_db::JsonDbWrapper,
    models::{CharacterDbAlone, CharacterDbEntry, UserDBEntry},
    schema::{characters, users},
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
/// gated. `userId` only sets the per-row `mine` flag (the list is server-wide);
/// in-memory only (cleared on arena-server restart) — durable history is #NB-3.
#[get("/blades.bgs.services/api/dev/v1/recent-matches")]
pub async fn recent_matches(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<RecentMatchesQuery>,
) -> Result<Json<Vec<RecentTicketView>>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let q = query.into_inner();
    let limit = q.limit.unwrap_or(25).min(100);
    Ok(Json(app_state.arena.recent.recent(limit, q.user_id)))
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
}
