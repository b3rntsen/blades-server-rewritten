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

#[get("/api/game/v1/public/characters/{character_id}/announcements")]
pub async fn get_announcements(app_state: web::Data<Arc<ServerGlobal>>) -> Json<AnnouncementsResponse> {
    // The captured retail news, plus one entry per arena season this build
    // configures. The season entries are generated from `arena_season::SEASONS`
    // rather than pasted into `announcements.json`, so the season's dates cannot
    // drift away from the season itself. The client filters by
    // `startTime`/`ttl`, so a future season's entry is inert until it opens.
    let mut announcements = app_state.static_data.announcements.clone();
    announcements.extend(crate::arena::arena_season::season_announcements());
    Json(AnnouncementsResponse { announcements })
}

#[cfg(test)]
mod tests {
    use crate::arena::arena_season::{SEASONS, season_announcements};

    /// The generated season entries must be indistinguishable in shape from the
    /// 156 captured retail ones, and must not collide with any of their ids.
    #[test]
    fn season_entries_are_shaped_like_the_captured_ones() {
        let generated = season_announcements();
        assert_eq!(generated.len(), SEASONS.len(), "one news entry per season");

        let captured: Vec<blades_lib::static_data::Announcement> = serde_json::from_str(
            include_str!("../../deploy/static/announcements.json"),
        )
        .expect("the captured announcements file parses");
        assert_eq!(captured.len(), 156, "precondition: the captured corpus is intact");

        for a in &generated {
            let v = serde_json::to_value(a).unwrap();
            let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
            keys.sort_unstable();
            assert_eq!(keys, ["assetUrl", "id", "startTime", "ttl", "type"]);
            assert_eq!(v["type"], "BASIC", "every captured record is BASIC");
            assert!(
                !captured.iter().any(|c| c.id == a.id),
                "generated id {} collides with a captured retail announcement",
                a.id
            );
            assert!(a.ttl > a.start_time);
        }
    }
}
