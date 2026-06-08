use std::sync::Arc;

use actix_web::{
    get,
    http::StatusCode,
    web::{self, Json},
};
use serde::Serialize;
use tokio::{fs::File, io::AsyncReadExt};

use crate::{BladeApiError, ServerGlobal};

#[derive(Serialize)]
struct GetTownResponse {
    town: serde_json::Value,
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/towns/current")]
pub async fn get_town(
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<Json<GetTownResponse>, BladeApiError> {
    // Serve the static default town. This used to unwrap() every step, so a
    // missing or invalid default_town.json PANICKED the actix worker — the
    // client then saw a dropped connection ("Communication/Network error")
    // rather than a clean HTTP error. Handle each failure as a 500 so the
    // worker survives and the cause is logged.
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
