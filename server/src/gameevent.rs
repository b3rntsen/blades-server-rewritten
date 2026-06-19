//! Game events (daily / Sigil quests) — `POST /gameevents`.
//!
//! Surfaces the currently-active event quests from the capture-derived library (see
//! [`crate::static_loader`] → `game_events.json`). A daily-rotating slice is stamped
//! with a current time window so 2-3 daily/Sigil quests read as available now.
//! Completing one pays Sigil via the existing quest flow.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    post,
    web::{self, Json},
};
use blades_lib::features::game_events::{self, GameEvent};
use serde::Serialize;

use crate::ServerGlobal;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GetGameEventsResponse {
    game_events: Vec<GameEvent>,
}

#[post("/blades.bgs.services/api/game/v1/public/characters/{character_id}/gameevents")]
pub async fn get_game_events(app_state: web::Data<Arc<ServerGlobal>>) -> Json<GetGameEventsResponse> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Json(GetGameEventsResponse {
        game_events: game_events::active_events(&app_state.static_data.game_events, now),
    })
}
