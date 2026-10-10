use std::collections::HashMap;

use actix_web::{
    get, post,
    web::{self, Json},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::session::SessionLookedUpMaybe;

#[post("/blades.bgs.services/api/analytics/v1/public/stats/client")]
pub async fn blades_bgs_stat_analytics() -> Json<Option<()>> {
    return Json(None);
}

/// One analytics batch, reduced to the two fields the server reads. Every other field
/// is skipped by serde and nothing of the batch is stored or forwarded.
#[derive(Deserialize)]
struct AnalyticsBatch {
    #[serde(default)]
    events: Vec<AnalyticsEvent>,
}

#[derive(Deserialize)]
struct AnalyticsEvent {
    #[serde(default)]
    payload: Option<AnalyticsPayload>,
}

#[derive(Deserialize)]
struct AnalyticsPayload {
    #[serde(default)]
    hero_uuid: Option<Uuid>,
    #[serde(default)]
    gear_rating: Option<i64>,
}

/// `(character, gear_rating)` of the LAST event in the batch that carries both — the
/// events are in the order the client logged them. `None` for a body that does not
/// parse: analytics must never fail a request.
fn latest_gear_rating(body: &[u8]) -> Option<(Uuid, u32)> {
    let batch: AnalyticsBatch = serde_json::from_slice(body).ok()?;
    batch.events.iter().rev().find_map(|event| {
        let payload = event.payload.as_ref()?;
        let rating = u32::try_from(payload.gear_rating?).ok().filter(|r| *r > 0)?;
        Some((payload.hero_uuid?, rating))
    })
}

/// The client's analytics batches. Nothing is kept but one number: every event carries
/// `hero_uuid` and `gear_rating`, the client's Effective Player Level
/// (`GearLevelParameters.GetEffectiveLevelForPlayer`) — the level it scores every Abyss
/// kill against. The server cannot compute that gear blend, and estimating it from the
/// character level left the server's reward gauge far behind any client whose gear sits
/// below the estimate (#360). Retail's own `/start` carried this exact number as
/// `initialPlayerLevel` (10, 11 and 75 beside the captured `/start`s).
///
/// Stored on the caller's own session only, so no one can set another player's.
#[post("/blades.bgs.services/api/analytics/v1/public/events")]
pub async fn blades_bgs_event_analytics(
    session: SessionLookedUpMaybe,
    body: Option<web::Bytes>,
) -> Json<Option<()>> {
    if let (Ok(session), Some(body)) = (session.get_session_or_error(), body) {
        if let Some((character_id, gear_rating)) = latest_gear_rating(&body) {
            session.session.record_gear_rating(character_id, gear_rating);
        }
    }
    Json(None)
}

#[post("/{server_id}.api.swrve.com/1/batch")]
pub async fn swrve_batch_submit() -> &'static str {
    ""
}

#[get("/{server_id}.content.swrve.com/api/1/user_resources_and_campaigns")]
pub async fn swrve_submit_device_info(
    _query: web::Query<HashMap<String, String>>,
) -> Json<HashMap<(), ()>> {
    Json(HashMap::new())
}

#[derive(Serialize, Debug)]
struct SwrveIdentifyResponse {
    status: &'static str,
    swrve_id: Uuid,
}

#[post("/{server_id}.identity.swrve.com/identify")]
pub async fn swrve_identity_identify() -> Json<SwrveIdentifyResponse> {
    Json(SwrveIdentifyResponse {
        status: "new_external_id",
        swrve_id: Uuid::new_v4(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppcenterLogResponse {
    status: &'static str,
    valid_diagnostics_ids: Vec<Uuid>,
    throttled_diagnostics_ids: Vec<Uuid>,
    correlation_id: Uuid,
}

#[post("/in.appcenter.ms/logs")]
pub async fn appcenter_log(
    _query: web::Query<HashMap<String, String>>,
) -> Json<AppcenterLogResponse> {
    Json(AppcenterLogResponse {
        status: "Success",
        valid_diagnostics_ids: Vec::new(),
        throttled_diagnostics_ids: Vec::new(),
        correlation_id: Uuid::new_v4(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of a captured retail batch (capture 12010, the request right before the
    /// ipl-10 `/start`), trimmed: the gear rating is the run's initialPlayerLevel.
    #[test]
    fn a_captured_batch_yields_the_heros_gear_rating() {
        let body = serde_json::json!({"events": [
            {"header": {"event_type": "item_info_collection", "platform_id": 4},
             "payload": {"hero_uuid": "78f2b668-97ff-45d0-99fa-7343fd059480", "hero_level": 7,
                         "gear_rating": 9, "item_uuid_list": ["f9a87486-39c1-49f8-9024-0c08ee090165"]}},
            {"header": {"event_type": "inventory_info"},
             "payload": {"hero_uuid": "78f2b668-97ff-45d0-99fa-7343fd059480", "hero_level": 7,
                         "gear_rating": 10, "size_total": 120}},
            {"header": {"event_type": "session_info"}, "payload": {"device_id": "x"}}
        ]});
        assert_eq!(
            latest_gear_rating(body.to_string().as_bytes()),
            Some((Uuid::parse_str("78f2b668-97ff-45d0-99fa-7343fd059480").unwrap(), 10))
        );
    }

    #[test]
    fn junk_or_a_batch_without_a_rating_yields_nothing() {
        assert_eq!(latest_gear_rating(b"not json"), None);
        assert_eq!(latest_gear_rating(br#"{"events": []}"#), None);
        assert_eq!(
            latest_gear_rating(br#"{"events": [{"payload": {"hero_uuid": null, "gear_rating": 10}}]}"#),
            None
        );
        assert_eq!(
            latest_gear_rating(
                br#"{"events": [{"payload": {"hero_uuid": "78f2b668-97ff-45d0-99fa-7343fd059480", "gear_rating": 0}}]}"#
            ),
            None
        );
    }
}
