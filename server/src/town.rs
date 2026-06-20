use std::sync::Arc;

use actix_web::{
    get,
    http::StatusCode,
    web::{self, Json},
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::RunQueryDsl;
use serde::Serialize;
use serde_json::Value;
use tokio::{fs::File, io::AsyncReadExt};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, json_db::JsonDbWrapper, models::CharacterDbEntryTown,
    session::SessionLookedUpMaybe, util::get_only_single_character_and_check_permission,
};

#[derive(Serialize)]
struct GetTownResponse {
    town: serde_json::Value,
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/towns/current")]
pub async fn get_town(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<GetTownResponse>, BladeApiError> {
    let character_id = path.into_inner();

    // Best-effort personalization: serve the requesting character's OWN captured
    // town when we have one. Any miss (no session, character not found, not owned,
    // or no stored town) falls through to the static default — serving the town
    // must never regress the menu/town load into an error.
    if let Some(town) = load_personal_town(&session, &app_state, character_id).await {
        return Ok(Json(GetTownResponse { town }));
    }

    // Fallback: the static default town (previously the ONLY behaviour). This used
    // to unwrap() every step, so a missing/invalid default_town.json PANICKED the
    // actix worker and the client saw a dropped connection ("Communication/Network
    // error"). Handle each failure as a 500 so the worker survives and logs why.
    let path = app_state.static_data_path.join("default_town.json");
    let mut file = File::open(&path).await.map_err(|e| {
        eprintln!("[town] cannot open {path:?}: {e}");
        BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 3, 0)
    })?;
    let mut content = String::new();
    file.read_to_string(&mut content).await.map_err(|e| {
        eprintln!("[town] cannot read {path:?}: {e}");
        BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 3, 0)
    })?;
    let town = serde_json::from_str(&content).map_err(|e| {
        eprintln!("[town] invalid json in {path:?}: {e}");
        BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 3, 0)
    })?;
    Ok(Json(GetTownResponse { town }))
}

/// Look up the character's stored, ownership-checked town. Returns `None` on any
/// miss (no session / not found / not owned / null town / db error) so the caller
/// falls back to the default town.
async fn load_personal_town(
    session: &SessionLookedUpMaybe,
    app_state: &ServerGlobal,
    character_id: Uuid,
) -> Option<Value> {
    let session = session.get_session_or_error().ok()?;
    let mut conn = app_state.db_pool.get().await.ok()?;
    let rows = {
        use crate::schema::characters::dsl::*;
        characters
            .filter(id.eq(character_id))
            .select(CharacterDbEntryTown::as_select())
            .load(&mut conn)
            .await
            .ok()?
    };
    let entry = get_only_single_character_and_check_permission(rows, &session.session).ok()?;
    match entry.town {
        Some(JsonDbWrapper(v)) if !v.is_null() => Some(v),
        _ => None,
    }
}
