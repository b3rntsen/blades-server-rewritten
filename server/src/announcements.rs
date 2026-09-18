//! In-game news — `GET /…/announcements`.
//!
//! Returns capture-derived news plus any unclaimed Arena Reset gift belonging
//! to this character. Arena seasons live in PostgreSQL, so their announcements
//! must come from that same source instead of the obsolete compile-time season.

use std::sync::Arc;

use actix_web::{
    get,
    web::{self, Json},
};
use blades_lib::static_data::Announcement;
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::RunQueryDsl;
use serde::Serialize;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, arena::season_store, models::CharacterDbEntryServerState,
    session::SessionLookedUpMaybe, util::get_only_single_character_and_check_permission,
};

#[derive(Serialize)]
struct AnnouncementsResponse {
    announcements: Vec<Announcement>,
}

#[get("/blades.bgs.services/api/game/v1/public/characters/{character_id}/announcements")]
pub async fn get_announcements(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
) -> Result<Json<AnnouncementsResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    let owners = {
        use crate::schema::characters::dsl::*;
        characters
            .filter(id.eq(character_id))
            .select(CharacterDbEntryServerState::as_select())
            .load(&mut conn)
            .await?
    };
    get_only_single_character_and_check_permission(owners, &session.session)?;

    // One indexed existence probe per ended season. The award unique index
    // starts `(season_id, character_id)`, so this remains cheap without adding
    // a production index or running a migration for the feature.
    let pending: Vec<season_store::SeasonRow> = diesel::sql_query(
        "SELECT s.* FROM arena_seasons s \
         WHERE s.status = 'ended' AND s.ended_at IS NOT NULL \
           AND EXISTS ( \
             SELECT 1 FROM arena_season_awards a \
             WHERE a.season_id = s.id AND a.character_id = $1 \
               AND a.granted_at IS NULL \
           ) \
         ORDER BY s.ended_at DESC",
    )
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .load(&mut conn)
    .await?;

    let mut announcements = app_state.static_data.announcements.clone();
    announcements.extend(pending.iter().map(season_reward_announcement));

    // A Free for All the client cannot see is a giveaway nobody collects: gift
    // ids are only discoverable through this feed. Derived from the open run
    // rather than stored alongside it, so the advert and the gift cannot drift.
    if let Some(run) = crate::free_for_all::open_run(&mut conn, now_secs()).await {
        announcements.push(free_for_all_announcement(&run));
    }

    Ok(Json(AnnouncementsResponse { announcements }))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const REWARD_ANNOUNCEMENT_LIFETIME: i64 = 31 * 24 * 60 * 60;

/// The banner for an open Free for All.
///
/// `ttl` is the window's own close time, not a fixed lifetime: the banner must
/// not outlive the gift, or players tap Claim and get a "not active" error.
fn free_for_all_announcement(run: &crate::free_for_all::FreeForAllRun) -> Announcement {
    Announcement {
        id: run.id.to_string(),
        r#type: "BASIC".into(),
        start_time: run.opens_at,
        ttl: run.closes_at,
        // Namespaced like the season banner, but deliberately NOT carrying a
        // gift id: this one grants nothing on tap, so the edge serves the
        // namespace with an empty GlobalGiftId and the dialog renders a single OK.
        asset_url: format!(
            "https://announcements.blades.bgs.services/free-for-all/{}",
            run.id
        ),
    }
}

fn season_reward_announcement(season: &season_store::SeasonRow) -> Announcement {
    let start = season.ended_at.unwrap_or(season.ends_at);
    Announcement {
        id: season.id.to_string(),
        r#type: "BASIC".into(),
        start_time: start,
        ttl: start + REWARD_ANNOUNCEMENT_LIFETIME,
        // Both the VPN addon and no-VPN nginx route this namespaced path to a
        // generic Arena Reset presentation while inserting this season id into
        // its manifest's GlobalGiftId.
        asset_url: format!(
            "https://announcements.blades.bgs.services/arena-season/{}",
            season.id
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_run(opens_at: i64, closes_at: i64) -> crate::free_for_all::FreeForAllRun {
        crate::free_for_all::FreeForAllRun {
            id: Uuid::from_u128(1),
            opens_at,
            closes_at,
            gems: 100,
            multiplier: 1,
            cadence: "first_saturday".into(),
            opened_by: None,
            note: None,
            created_at: opens_at,
        }
    }

    #[test]
    fn the_giveaway_banner_is_its_own_namespace_and_carries_no_gift() {
        // Gems are earned by fighting, not claimed, so this must not reuse the
        // season namespace — whose trailing uuid the edge copies into
        // GlobalGiftId to arm a Claim button.
        let a = free_for_all_announcement(&a_run(100, 200));
        assert!(
            a.asset_url
                .ends_with(&format!("/free-for-all/{}", Uuid::from_u128(1)))
        );
        assert!(!a.asset_url.contains("arena-season"));
    }

    #[test]
    fn the_banner_expires_with_the_window() {
        // A banner that outlives the gift is a Claim button that errors.
        let a = free_for_all_announcement(&a_run(100, 200));
        assert_eq!(a.start_time, 100);
        assert_eq!(a.ttl, 200);
    }

    #[test]
    fn reward_entry_uses_the_db_season_id_for_asset_and_gift_routing() {
        let id = Uuid::from_u128(42);
        let season = season_store::SeasonRow {
            id,
            number: 4,
            name: "Season 4".into(),
            starts_at: 100,
            ends_at: 200,
            status: "ended".into(),
            scoring: "shipped".into(),
            reset_rule: "hard_reset".into(),
            created_at: 50,
            ended_at: Some(210),
        };
        let a = season_reward_announcement(&season);
        assert_eq!(a.id, id.to_string());
        assert!(a.asset_url.ends_with(&format!("/arena-season/{id}")));
        assert_eq!(a.start_time, 210);
        assert_eq!(a.ttl, 210 + REWARD_ANNOUNCEMENT_LIFETIME);
    }
}
