//! In-game news — `GET /…/announcements`.
//!
//! Returns the server's news list. The entries are capture-derived
//! ([`crate::static_loader`] → `announcements.json`); each carries an `assetUrl` on
//! Bethesda's (now-defunct) announcement CDN, so the client shows the entry but
//! quietly fails to load its banner. Served as-is (the client filters by
//! `startTime`/`ttl` itself).

use std::sync::Arc;

use actix_web::{
    get,
    web::{self, Json},
};
use blades_lib::static_data::Announcement;
use serde::Serialize;

use crate::ServerGlobal;

#[derive(Serialize)]
struct AnnouncementsResponse {
    announcements: Vec<Announcement>,
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/announcements")]
pub async fn get_announcements(app_state: web::Data<Arc<ServerGlobal>>) -> Json<AnnouncementsResponse> {
    Json(AnnouncementsResponse {
        announcements: app_state.static_data.announcements.clone(),
    })
}
