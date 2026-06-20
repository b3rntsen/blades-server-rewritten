//! Game events (daily / Sigil quests) — `POST /gameevents`.
//!
//! Bethesda advertises a rotating set of timed event quests; completing one pays Sigil
//! (the event currency) via the normal quest flow. The full event library is
//! capture-derived (the union of every event seen); the endpoint surfaces a few as
//! *active now* by stamping a current time window onto them, so 2-3 daily/Sigil quests
//! appear available over the next day or two.
//!
//! Captured event:
//! ```jsonc
//! { "gameEventInstanceId": "b483c668-…::1777780800", "type": "quest",
//!   "startTimeSecs": 1777780800, "endTimeSecs": 1777953600,
//!   "recurrence": { "recurrenceType": "daily", "startTimeSecs": 1663214400,
//!                   "durationSecs": 172800, "recurrenceInterval": 39 },
//!   "questId": "7f0d1508-…", "important": true }
//! ```
//!
//! NOTE: advertising an event is faithful; *playing* it still needs the quest's
//! dungeon definition in `GameData` (a separate gap — `parsed.json` ships empty).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How many events to surface as active at once.
pub const ACTIVE_EVENT_COUNT: usize = 3;
/// Default active window if an event template carries no instance duration (2 days,
/// matching the observed `durationSecs`).
const DEFAULT_WINDOW_SECS: i64 = 172_800;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Recurrence {
    pub recurrence_type: String,
    pub start_time_secs: i64,
    pub duration_secs: i64,
    pub recurrence_interval: i64,
}

/// A capture-derived event template (one quest event, its recurrence + how long an
/// instance stays open).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EventDef {
    pub event_id: Uuid,
    pub quest_id: Uuid,
    pub recurrence: Recurrence,
    #[serde(default)]
    pub important: bool,
    /// How long one active instance lasts (captured `endTimeSecs - startTimeSecs`).
    #[serde(default)]
    pub instance_duration_secs: i64,
}

/// One active event on the wire.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GameEvent {
    pub game_event_instance_id: String,
    pub r#type: String,
    pub start_time_secs: i64,
    pub end_time_secs: i64,
    pub recurrence: Recurrence,
    pub quest_id: Uuid,
    pub important: bool,
}

/// Pick the events active right now: a daily-rotating slice of the library, each
/// stamped with a window `[period_start, period_start + duration]` so they read as
/// currently available.
pub fn active_events(library: &[EventDef], now_secs: i64) -> Vec<GameEvent> {
    if library.is_empty() {
        return Vec::new();
    }
    let period = now_secs.div_euclid(86_400);
    let period_start = period * 86_400;
    let count = ACTIVE_EVENT_COUNT.min(library.len());
    (0..count)
        .map(|i| {
            let def = &library[((period as usize).wrapping_add(i)) % library.len()];
            let duration = if def.instance_duration_secs > 0 {
                def.instance_duration_secs
            } else {
                DEFAULT_WINDOW_SECS
            };
            GameEvent {
                game_event_instance_id: format!("{}::{}", def.event_id, period_start),
                r#type: "quest".to_string(),
                start_time_secs: period_start,
                end_time_secs: period_start + duration,
                recurrence: def.recurrence.clone(),
                quest_id: def.quest_id,
                important: def.important,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(n: u128) -> EventDef {
        EventDef {
            event_id: Uuid::from_u128(n),
            quest_id: Uuid::from_u128(n + 1000),
            recurrence: Recurrence {
                recurrence_type: "daily".to_string(),
                start_time_secs: 1663214400,
                duration_secs: 172800,
                recurrence_interval: 39,
            },
            important: true,
            instance_duration_secs: 172800,
        }
    }

    #[test]
    fn surfaces_up_to_three_active_in_a_current_window() {
        let lib: Vec<EventDef> = (0..10).map(def).collect();
        let now = 1_777_800_000;
        let active = active_events(&lib, now);
        assert_eq!(active.len(), 3);
        for e in &active {
            assert!(e.start_time_secs <= now && now < e.end_time_secs, "window covers now");
            assert!(e.game_event_instance_id.contains("::"));
            assert_eq!(e.r#type, "quest");
            // window is ~1-2 days
            assert_eq!(e.end_time_secs - e.start_time_secs, 172800);
        }
    }

    #[test]
    fn rotation_changes_across_days_but_is_stable_within_a_day() {
        let lib: Vec<EventDef> = (0..10).map(def).collect();
        let day1 = active_events(&lib, 1_777_800_000);
        let day1b = active_events(&lib, 1_777_800_000 + 3600);
        let day2 = active_events(&lib, 1_777_800_000 + 86_400);
        assert_eq!(
            day1[0].game_event_instance_id, day1b[0].game_event_instance_id,
            "stable within a day"
        );
        assert_ne!(
            day1[0].game_event_instance_id, day2[0].game_event_instance_id,
            "rotates next day"
        );
    }

    #[test]
    fn empty_library_yields_nothing() {
        assert!(active_events(&[], 1_777_800_000).is_empty());
    }
}
