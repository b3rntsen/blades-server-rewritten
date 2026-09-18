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
use blades_lib::features::game_events::{self, EventDef, GameEvent};
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
    // MEASURED, not assumed: retail's /gameevents returns TWO OPEN EVENTS PLUS
    // THE NEXT ONE TO OPEN. Across all 36 distinct captured responses, 35 carry
    // exactly three events and every one of those is 2 ACTIVE + 1 FUTURE, with
    // the future one 4.1-21.1 h away — inside `WARNING_LEAD_SECS`. (The 36th is
    // empty.) We returned only the open two, so the events screen could never
    // show players what was coming next.
    //
    // This is the same "starting soon" set /quests advertises as
    // `gameEventQuestsInWarning`, which is why it reuses `upcoming_events`
    // rather than growing a second notion of soon.
    Json(GetGameEventsResponse {
        game_events: response_events(&app_state.static_data.game_events, now),
    })
}

/// What the endpoint puts on the wire: the open events, then the next one to
/// open. Lifted out of the handler so it can be tested — the handler is the
/// place a composition like this silently stops being what retail sent.
fn response_events(library: &[EventDef], now: i64) -> Vec<GameEvent> {
    let mut out = game_events::active_events(library, now);
    out.extend(game_events::upcoming_events(
        library,
        now,
        game_events::WARNING_LEAD_SECS,
    ));
    out.sort_by_key(|e| (e.start_time_secs, e.game_event_instance_id.clone()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape retail put on the wire, against the committed calendar.
    ///
    /// Of the 36 distinct `/gameevents` responses in the capture corpus, 35 carry
    /// exactly three events and every one is 2 open + 1 about to open (4.1-21.1 h
    /// out); the 36th is empty. We used to send only the open two, so the events
    /// screen could never show what was coming.
    #[test]
    fn the_endpoint_sends_the_two_open_events_and_the_next_one() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/game_events.json");
        let raw = std::fs::read_to_string(&path).expect("read game_events.json");
        let lib: Vec<EventDef> = serde_json::from_str(&raw).expect("valid game_events.json");

        // A full cycle of days, skipping the holiday windows — those are ours,
        // not retail's, and they deliberately add a third open event.
        let start = 1_777_800_000i64;
        let mut checked = 0;
        for day in 0..44 {
            let now = start + day * 86_400;
            if game_events::active_events(&lib, now)
                .len()
                != game_events::active_events(
                    &lib.iter().filter(|d| !d.annual).cloned().collect::<Vec<_>>(),
                    now,
                )
                .len()
            {
                continue; // a holiday event is open; not the retail case
            }
            let out = response_events(&lib, now);
            assert_eq!(out.len(), 3, "day {day}: retail always sent three");
            let open = out.iter().filter(|e| e.start_time_secs <= now).count();
            let soon = out.iter().filter(|e| e.start_time_secs > now).count();
            assert_eq!(open, 2, "day {day}: two open");
            assert_eq!(soon, 1, "day {day}: one about to open");
            // …and the one to come is within a day, as every capture showed.
            let next = out.iter().find(|e| e.start_time_secs > now).unwrap();
            assert!(
                next.start_time_secs - now <= 86_400,
                "day {day}: the upcoming event is {}h out",
                (next.start_time_secs - now) / 3600
            );
            checked += 1;
        }
        assert!(checked >= 40, "only {checked} retail-shaped days sampled");
    }

    /// CONTROL: dropping the upcoming half gives the OLD behaviour — two events,
    /// never three. Without this the test above would still pass if
    /// `upcoming_events` silently returned the same set twice.
    #[test]
    fn the_open_events_alone_are_only_two() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/game_events.json");
        let raw = std::fs::read_to_string(&path).expect("read game_events.json");
        let all: Vec<EventDef> = serde_json::from_str(&raw).expect("valid game_events.json");
        let lib: Vec<EventDef> = all.into_iter().filter(|d| !d.annual).collect();
        let now = 1_777_800_000i64;
        assert_eq!(game_events::active_events(&lib, now).len(), 2);
        assert_eq!(response_events(&lib, now).len(), 3);
    }
}
