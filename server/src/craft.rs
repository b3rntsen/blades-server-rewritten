use actix_web::{get, web::Json};
use serde::Serialize;
use serde_json::Value;

/// The blacksmith/crafting "active jobs" list. The real game returns
/// `{ "crafts": [ { id, buildingId, craftingTypeId, completedAt, results, … } ] }`;
/// the emulator persists no craft jobs yet, so this is always empty. (The earlier
/// stub returned a bare `{}` — the wrong shape; the repair gate reads this list.)
#[derive(Serialize)]
struct GetCraftsResponse {
    crafts: Vec<Value>,
}

#[get("blades.bgs.services/api/game/v1/public/characters/{character_id}/crafts")]
pub async fn get_crafts() -> Json<GetCraftsResponse> {
    Json(GetCraftsResponse { crafts: Vec::new() })
}
