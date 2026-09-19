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
//! The recurrence is real, not a slice. Every one of the 39 captured events repeats
//! on a `recurrenceInterval` of 39 days with a `durationSecs` window of 172 800 (2
//! days), so at any instant `39 events x 2/39 days` puts an expected **2** events in
//! their window — and retail's captures show exactly 1 or 2 active (2 in 614
//! responses, 1 in 43). The same arithmetic puts exactly 1 event within a day of
//! opening, and retail's `gameEventQuestsInWarning` array had exactly 1 entry in all
//! 686 responses that carried one. An earlier version of this module ignored
//! `recurrenceInterval` and surfaced a rotating slice of 3 instead.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Default active window if an event template carries no instance duration (2 days,
/// matching the observed `durationSecs`).
const DEFAULT_WINDOW_SECS: i64 = 172_800;
const SECS_PER_DAY: i64 = 86_400;

/// How far ahead of its opening an event is announced in `gameEventQuestsInWarning`.
///
/// MEASURED: over the 686 retail `/quests` responses carrying a warning entry, the
/// lead time `startTimeSecs - now` ran from 0.1 h to exactly 24.0 h and never above.
pub const WARNING_LEAD_SECS: i64 = SECS_PER_DAY;

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
    /// Repeat on the anchor's CALENDAR DATE every year, instead of every
    /// `recurrenceInterval` days.
    ///
    /// Every captured event repeats on a fixed day count, so retail never needed
    /// this — but a holiday does not fall on a fixed day count. A 365-day period
    /// loses a day at every leap year, so an event anchored to 31 October drifts to
    /// the 30th within four years and off the holiday entirely within a couple of
    /// decades. The wire object still advertises `recurrenceInterval: 365`, which is
    /// what the client understands; only our choice of window start differs.
    #[serde(default)]
    pub annual: bool,
    /// A deliberate, self-expiring extra window that is NOT retail's schedule.
    ///
    /// Marked so the tests that assert retail's shape can exclude it by meaning
    /// rather than by name, and so nobody later reads it as a measurement. A
    /// preview uses `recurrenceInterval: 0`, which makes it one-shot: it opens at
    /// its anchor, closes after its window and never returns, so a forgotten
    /// preview cannot quietly become part of the calendar.
    #[serde(default)]
    pub preview: bool,
}

/// Days from 1970-01-01 to `y-m-d` (proleptic Gregorian). Howard Hinnant's
/// `days_from_civil`, which is exact for every date we can be handed and needs no
/// date crate in this library.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m as i64 - 3 } else { m as i64 + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Midnight UTC on `y-m-d`, clamping a 29 February anchor to the 28th in a common
/// year so an anchor can never silently skip a year.
fn utc_midnight(y: i64, m: u32, d: u32) -> i64 {
    let mut d = d;
    if m == 2 && d == 29 && days_from_civil(y, 3, 1) - days_from_civil(y, 2, 1) == 28 {
        d = 28;
    }
    days_from_civil(y, m, d) * SECS_PER_DAY
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

impl EventDef {
    /// How long one instance stays open.
    pub fn window_secs(&self) -> i64 {
        if self.instance_duration_secs > 0 {
            self.instance_duration_secs
        } else if self.recurrence.duration_secs > 0 {
            self.recurrence.duration_secs
        } else {
            DEFAULT_WINDOW_SECS
        }
    }

    /// The gap between two consecutive instances, in seconds.
    fn period_secs(&self) -> i64 {
        let days = self.recurrence.recurrence_interval;
        if days > 0 { days * SECS_PER_DAY } else { 0 }
    }

    /// Start of the instance that most recently began at or before `now`.
    ///
    /// `None` when the series has not started yet, or when the event does not repeat
    /// (interval 0) and its single window already lies in the future.
    pub fn instance_start_at_or_before(&self, now: i64) -> Option<i64> {
        let anchor = self.recurrence.start_time_secs;
        if now < anchor {
            return None;
        }
        if self.annual {
            let (_, am, ad) = civil_from_days(anchor.div_euclid(SECS_PER_DAY));
            let (ny, ..) = civil_from_days(now.div_euclid(SECS_PER_DAY));
            let this_year = utc_midnight(ny, am, ad);
            return Some(if this_year <= now {
                this_year
            } else {
                utc_midnight(ny - 1, am, ad)
            });
        }
        let period = self.period_secs();
        if period <= 0 {
            return Some(anchor);
        }
        Some(anchor + ((now - anchor) / period) * period)
    }

    /// Start of the first instance that begins strictly after `now`.
    pub fn next_instance_start_after(&self, now: i64) -> Option<i64> {
        let anchor = self.recurrence.start_time_secs;
        if now < anchor {
            return Some(anchor);
        }
        if self.annual {
            let (_, am, ad) = civil_from_days(anchor.div_euclid(SECS_PER_DAY));
            let (ny, ..) = civil_from_days(now.div_euclid(SECS_PER_DAY));
            let this_year = utc_midnight(ny, am, ad);
            return Some(if this_year > now {
                this_year
            } else {
                utc_midnight(ny + 1, am, ad)
            });
        }
        let period = self.period_secs();
        if period <= 0 {
            return None; // one-shot event, already begun
        }
        Some(anchor + ((now - anchor) / period + 1) * period)
    }

    /// The instance covering `now`, if the event is open.
    pub fn active_instance_start(&self, now: i64) -> Option<i64> {
        let start = self.instance_start_at_or_before(now)?;
        (now < start + self.window_secs()).then_some(start)
    }

    /// Build the wire object for the instance beginning at `start`.
    pub fn instance(&self, start: i64) -> GameEvent {
        GameEvent {
            game_event_instance_id: format!("{}::{}", self.event_id, start),
            r#type: "quest".to_string(),
            start_time_secs: start,
            end_time_secs: start + self.window_secs(),
            recurrence: self.recurrence.clone(),
            quest_id: self.quest_id,
            important: self.important,
        }
    }
}

/// The events whose instance window covers `now`.
/// The most event quests retail ever had open at once.
///
/// MEASURED over the pre-shutdown corpus: `gameEventQuests[]` carried two entries
/// in 614 `/quests` responses and one in 43. Never three. The arithmetic behind it
/// is retail's own — 39 events, one opening per day, each open for two days.
///
/// This is a WIRE cap, not a calendar change. The calendar may legitimately have
/// three open: a seasonal event runs a SEVEN-day window (#189/#304) across a
/// one-per-day cadence, so for the whole week it runs there is always a third.
/// The client's quest screen is retail's and was never given that case.
pub const MAX_CONCURRENT_EVENTS: usize = 2;

/// The events open right now, capped at what retail ever served.
///
/// Truncated from the FRONT after sorting by start time, so the two most recently
/// opened win. That is what retail's pair always was — "opened today" and "opened
/// yesterday" — and it stops a long seasonal window from permanently occupying a
/// slot and starving the daily rotation the player is meant to see turning over.
/// THE one place that decides which event instances are open right now.
///
/// Both callers must agree: `active_events` builds the `/gameevents` feed from
/// this, and `event_quests::mint` builds the per-character `GAME_EVENT` rows that
/// become `gameEventQuests[]`. They used to compute it separately — `mint` called
/// `active_instance_start` on each def itself — so a cap added to one did nothing
/// to the other. That is exactly the bug this function exists to make impossible,
/// and `mint_and_active_events_agree` pins it.
///
/// Returns `(instance_start, def)` sorted by start, already capped.
///
/// **Which two survive: the ones with the most time left.** The instance closest
/// to expiring is dropped, because it is the one a player has least opportunity
/// to act on and it would have gone on its own within hours.
///
/// The obvious alternative — keep the two most recently OPENED — is wrong, and
/// `the_holiday_events_open_on_their_holidays_and_stay_shut_otherwise` caught it:
/// an annual event opens at midnight alongside that day's ordinary rotation, so
/// "newest" evicted Season of the Witch on Halloween. A rare seasonal event must
/// not be displaced by a routine daily one.
pub fn open_instances<'a>(library: &'a [EventDef], now_secs: i64) -> Vec<(i64, &'a EventDef)> {
    let mut out: Vec<(i64, &EventDef)> = library
        .iter()
        .filter_map(|def| Some((def.active_instance_start(now_secs)?, def)))
        .collect();
    if out.len() > MAX_CONCURRENT_EVENTS {
        // Annual first, then most recently opened; `event_id` breaks ties so the
        // choice is stable rather than dependent on library order.
        out.sort_by(|a, b| {
            b.1.annual
                .cmp(&a.1.annual)
                .then_with(|| b.0.cmp(&a.0))
                .then_with(|| a.1.event_id.cmp(&b.1.event_id))
        });
        out.truncate(MAX_CONCURRENT_EVENTS);
    }
    // The wire is ordered by start, whatever the selection was.
    out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.event_id.cmp(&b.1.event_id)));
    out
}

pub fn active_events(library: &[EventDef], now_secs: i64) -> Vec<GameEvent> {
    open_instances(library, now_secs)
        .into_iter()
        .map(|(start, def)| def.instance(start))
        .collect()
}

/// The events whose next instance opens within `lead_secs` of `now` — retail's
/// `gameEventQuestsInWarning`, i.e. "starting soon", not "ending soon".
pub fn upcoming_events(library: &[EventDef], now_secs: i64, lead_secs: i64) -> Vec<GameEvent> {
    let mut out: Vec<GameEvent> = library
        .iter()
        .filter_map(|def| {
            let start = def.next_instance_start_after(now_secs)?;
            (start - now_secs <= lead_secs).then(|| def.instance(start))
        })
        .collect();
    out.sort_by_key(|e| (e.start_time_secs, e.game_event_instance_id.clone()));
    out
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
            annual: false,
            preview: false,
        }
    }

    const ANCHOR: i64 = 1_663_214_400;
    const PERIOD: i64 = 39 * 86_400;
    const WINDOW: i64 = 172_800;

    #[test]
    fn an_event_is_open_only_inside_its_recurring_window() {
        let d = def(1);
        // Inside the very first window.
        assert_eq!(d.active_instance_start(ANCHOR), Some(ANCHOR));
        assert_eq!(d.active_instance_start(ANCHOR + WINDOW - 1), Some(ANCHOR));
        // One second after it closes, and for the 37 days that follow, it is shut.
        assert_eq!(d.active_instance_start(ANCHOR + WINDOW), None);
        assert_eq!(d.active_instance_start(ANCHOR + PERIOD - 1), None);
        // ...and open again exactly one period later. This is the assertion the old
        // day-slice implementation could not pass: it ignored recurrenceInterval and
        // surfaced three events every single day regardless of their windows.
        assert_eq!(
            d.active_instance_start(ANCHOR + PERIOD),
            Some(ANCHOR + PERIOD),
            "the series repeats on recurrenceInterval days"
        );
        assert!(active_events(&[d.clone()], ANCHOR + WINDOW).is_empty());
        assert_eq!(active_events(&[d], ANCHOR + PERIOD).len(), 1);
    }

    #[test]
    fn an_event_that_has_not_started_yet_is_not_active() {
        let d = def(1);
        assert_eq!(d.active_instance_start(ANCHOR - 1), None);
        assert_eq!(d.next_instance_start_after(ANCHOR - 1), Some(ANCHOR));
    }

    #[test]
    fn the_wire_instance_is_stamped_with_its_own_window() {
        let d = def(1);
        let e = d.instance(ANCHOR + PERIOD);
        assert_eq!(e.game_event_instance_id, format!("{}::{}", d.event_id, ANCHOR + PERIOD));
        assert_eq!(e.start_time_secs, ANCHOR + PERIOD);
        assert_eq!(e.end_time_secs, ANCHOR + PERIOD + WINDOW);
        assert_eq!(e.r#type, "quest");
        assert_eq!(e.quest_id, d.quest_id);
    }

    #[test]
    fn warning_lists_what_opens_within_the_lead_and_nothing_else() {
        let d = def(1);
        let just_inside = ANCHOR - WARNING_LEAD_SECS;
        let just_outside = ANCHOR - WARNING_LEAD_SECS - 1;
        assert_eq!(
            upcoming_events(&[d.clone()], just_inside, WARNING_LEAD_SECS).len(),
            1,
            "an event opening in exactly 24h is announced"
        );
        assert!(
            upcoming_events(&[d.clone()], just_outside, WARNING_LEAD_SECS).is_empty(),
            "one second earlier it is not"
        );
        // An event that is currently OPEN is not also announced — the entry points at
        // its *next* window, which is a period away.
        let up = upcoming_events(&[d], ANCHOR, WARNING_LEAD_SECS);
        assert!(up.is_empty(), "an open event is not in the warning list");
    }

    #[test]
    fn empty_library_yields_nothing() {
        assert!(active_events(&[], 1_777_800_000).is_empty());
        assert!(upcoming_events(&[], 1_777_800_000, WARNING_LEAD_SECS).is_empty());
    }

    /// An annual event lands on its CALENDAR DATE, not 365 days later (#189).
    ///
    /// This is the whole reason `annual` exists. A 365-day period is a day short of
    /// a year whenever a leap day falls inside it, so a Halloween event anchored to
    /// 24 October opens on the 23rd from 2028, the 22nd from 2032, and has walked
    /// off the holiday entirely inside a working lifetime. Asserting a date rather
    /// than an offset is what catches that.
    #[test]
    fn an_annual_event_keeps_its_calendar_date_across_leap_years() {
        const DAY: i64 = 86_400;
        // 2026-10-24, 2027-10-24, 2028-10-24 (2028 is a leap year), 2029-10-24.
        let oct24 = [1_792_800_000i64, 1_824_336_000, 1_855_958_400, 1_887_494_400];

        let mut d = def(1);
        d.annual = true;
        d.recurrence.start_time_secs = oct24[0];
        d.recurrence.recurrence_interval = 365;
        d.recurrence.duration_secs = 8 * DAY;
        d.instance_duration_secs = 8 * DAY;

        for (i, &start) in oct24.iter().enumerate() {
            assert_eq!(
                d.active_instance_start(start + DAY),
                Some(start),
                "year {i}: the instance must begin on 24 October"
            );
            // Shut in between: midsummer of the following year.
            assert_eq!(d.active_instance_start(start + 240 * DAY), None, "year {i}: open in June");
        }

        // The same anchor under the old day-count rule drifts, which is what this
        // field exists to avoid — the control that proves the test can tell them apart.
        let mut drifting = d.clone();
        drifting.annual = false;
        assert_eq!(
            drifting.active_instance_start(oct24[2] + DAY),
            Some(oct24[2] - DAY),
            "365-day arithmetic should land a day early in 2028; if it does not, this \
             test is no longer proving anything"
        );
    }

    /// A 29 February anchor must still fire in a common year, on the 28th, rather
    /// than silently skipping three years out of four.
    #[test]
    fn a_leap_day_anchor_falls_back_to_the_28th() {
        const DAY: i64 = 86_400;
        let mut d = def(2);
        d.annual = true;
        d.recurrence.start_time_secs = 1_709_164_800; // 2024-02-29
        d.recurrence.duration_secs = 2 * DAY;
        d.instance_duration_secs = 2 * DAY;

        // 2025 has no 29th: the instance opens on the 28th (2025-02-28).
        assert_eq!(
            d.active_instance_start(1_740_700_800 + 3600),
            Some(1_740_700_800),
            "a leap-day anchor must not skip a common year"
        );
        // 2028 has one again.
        assert_eq!(d.active_instance_start(1_835_395_200 + 3600), Some(1_835_395_200));
    }

    /// The committed rotation, against what retail's captures actually show.
    ///
    /// MEASURED, from the 36 distinct `/gameevents` responses in the corpus: 35
    /// carry exactly three events, and every one of those is **2 open + 1 about
    /// to open**, the future one 4.1-21.1 h away. (The 36th is empty.) Every
    /// event in all 105 sightings has `recurrenceInterval: 39`,
    /// `durationSecs: 172800`, a 172 800 s instance window, and its own day —
    /// 39 events across 39 distinct anchors spanning exactly 38 days. Not one
    /// questId ever changed its recurrence.
    ///
    /// So retail's schedule is: **one event opens every day, each stays open two
    /// days**, which yields exactly two open and exactly one within a day of
    /// opening, forever. That invariant — not the number 39 — is what this
    /// asserts, because the library is now 44 events on a 44-day cycle (#189
    /// restored five retired ones) and the invariant survives that unchanged.
    #[test]
    fn the_rotation_keeps_two_open_and_one_about_to_open_every_day() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/game_events.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let all: Vec<EventDef> = serde_json::from_str(&raw).expect("valid game_events.json");
        let lib: Vec<EventDef> = all
            .iter()
            .filter(|d| !d.annual && !d.preview)
            .cloned()
            .collect();
        assert_eq!(
            all.iter().filter(|d| d.annual).count(),
            2,
            "the two annual holiday events"
        );

        // One event per day-slot of the cycle, which is what produces the shape.
        let period = lib[0].recurrence.recurrence_interval;
        assert_eq!(period as usize, lib.len(), "one event opens per day of the cycle");
        let mut slots: Vec<i64> = lib
            .iter()
            .map(|d| d.recurrence.start_time_secs.rem_euclid(period * SECS_PER_DAY) / SECS_PER_DAY)
            .collect();
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots.len(), lib.len(), "two events share a day-slot");

        // …and the observable consequence, sampled across a full cycle. Retail
        // never showed three open at once, so neither may we.
        let start = 1_777_800_000i64;
        for day in 0..period {
            let now = start + day * SECS_PER_DAY;
            let open = active_events(&lib, now).len();
            let soon = upcoming_events(&lib, now, WARNING_LEAD_SECS).len();
            assert_eq!(open, 2, "day {day}: {open} open, retail always showed 2");
            assert_eq!(soon, 1, "day {day}: {soon} about to open, retail always showed 1");
        }

        // The window itself is retail's, in every one of the 105 sightings.
        for d in &lib {
            assert_eq!(d.recurrence.duration_secs, 172_800);
            assert_eq!(d.window_secs(), 172_800);
        }
    }

    /// A preview window opens once and never comes back (#189).
    ///
    /// `recurrenceInterval: 0` is what makes that true, and it is the whole
    /// safety property: a preview nobody remembers to delete must not quietly
    /// become a second annual event. This asserts it never reopens rather than
    /// trusting the comment in the data.
    #[test]
    fn a_preview_window_opens_once_and_never_returns() {
        const DAY: i64 = 86_400;
        let mut d = def(7);
        d.preview = true;
        d.annual = false;
        d.recurrence.start_time_secs = 1_800_000_000;
        d.recurrence.recurrence_interval = 0;
        d.recurrence.duration_secs = 7 * DAY;
        d.instance_duration_secs = 7 * DAY;

        let anchor = 1_800_000_000i64;
        assert_eq!(d.active_instance_start(anchor), Some(anchor), "open at the anchor");
        assert_eq!(d.active_instance_start(anchor + 6 * DAY), Some(anchor), "still open on day 6");
        assert_eq!(d.active_instance_start(anchor + 7 * DAY), None, "shut when the window ends");
        // …and stays shut. A year, and five years, later.
        assert_eq!(d.active_instance_start(anchor + 365 * DAY), None);
        assert_eq!(d.active_instance_start(anchor + 5 * 365 * DAY), None);
        assert_eq!(d.next_instance_start_after(anchor + 8 * DAY), None, "nothing follows it");

        // CONTROL: the same definition with a real interval DOES come back, so
        // the assertions above are about `recurrenceInterval: 0` and not about
        // the event being broken.
        let mut recurring = d.clone();
        recurring.recurrence.recurrence_interval = 365;
        assert_eq!(
            recurring.active_instance_start(anchor + 365 * DAY),
            Some(anchor + 365 * DAY),
            "a recurring event reopens; the preview must not"
        );
    }
}

/// The client is never handed more open event quests than retail ever served.
///
/// Reported from a real device: the quest screen spun. Reproduced by replaying
/// that player's exact character and quest rows against the build before and
/// after the day's deploys — the only behavioural change in `/quests` besides the
/// XP tables was `gameEventQuests` going from 2 to 3.
///
/// The third instance is real, not a calendar bug: a seasonal event runs a
/// SEVEN-day window (#189/#304) across retail's one-opening-per-day cadence, so
/// for the whole week it runs there is always a third open. Retail's own
/// `gameEventQuests[]` carried two entries in 614 captured responses and one in
/// 43, never three, and the quest screen is retail's.
#[cfg(test)]
mod never_serve_more_open_events_than_retail {
    use super::*;

    fn library() -> Vec<EventDef> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/game_events.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid game_events.json")
    }

    /// How many instances the calendar has open, before any cap.
    fn uncapped(lib: &[EventDef], now: i64) -> usize {
        lib.iter()
            .filter(|d| d.active_instance_start(now).is_some())
            .count()
    }

    /// The window the seasonal event is actually open — found by scanning rather
    /// than hard-coded, so this keeps working when the calendar moves.
    fn a_day_with_three_open(lib: &[EventDef]) -> Option<i64> {
        let day = 86_400i64;
        let start = 1_789_000_000i64;
        (0..400).map(|d| start + d * day).find(|&t| uncapped(lib, t) >= 3)
    }

    #[test]
    fn a_third_open_instance_is_never_advertised() {
        let lib = library();
        let day = a_day_with_three_open(&lib).expect(
            "THE CONTROL: no day in the scanned year has three open, so the cap \
             would be untested and this test would prove nothing",
        );
        assert!(uncapped(&lib, day) >= 3, "the calendar really does have three");
        assert_eq!(
            active_events(&lib, day).len(),
            MAX_CONCURRENT_EVENTS,
            "but only two are served"
        );
    }

    /// THE INVARIANT THE FIRST FIX MISSED.
    ///
    /// `event_quests::mint` used to compute the open set itself, so capping
    /// `active_events` changed the `/gameevents` feed and left `gameEventQuests[]`
    /// at three — verified against a running server before this was written. Both
    /// now go through `open_instances`, and this is what stops them drifting apart
    /// again.
    #[test]
    fn open_instances_is_the_only_definition_of_open() {
        let lib = library();
        let day = 86_400i64;
        for d in 0..120 {
            let now = 1_789_000_000i64 + d * day;
            let from_open: Vec<i64> = open_instances(&lib, now).iter().map(|(s, _)| *s).collect();
            let from_active: Vec<i64> = active_events(&lib, now)
                .iter()
                .map(|e| e.start_time_secs)
                .collect();
            assert_eq!(
                from_open, from_active,
                "day {d}: the feed and the shared selection must be the same set"
            );
            assert!(from_open.len() <= MAX_CONCURRENT_EVENTS, "day {d}");
        }
    }

    #[test]
    fn an_ordinary_day_is_unchanged() {
        let lib = library();
        let day = 86_400i64;
        let mut ordinary = 0;
        for d in 0..120 {
            let now = 1_789_000_000i64 + d * day;
            let n = uncapped(&lib, now);
            if n <= MAX_CONCURRENT_EVENTS {
                assert_eq!(active_events(&lib, now).len(), n, "day {d}");
                ordinary += 1;
            }
        }
        assert!(ordinary > 0, "the window must contain ordinary days too");
    }

    /// An annual event is never evicted by the daily rotation — a player has a
    /// single window a year to play it.
    ///
    /// Checked on EVERY day of a year, not one sampled day, because two earlier
    /// policies each passed a single-day check and failed elsewhere: "keep the
    /// most recently opened" dropped Season of the Witch on Halloween, and "drop
    /// the one ending soonest" dropped it late in its run.
    #[test]
    fn an_annual_event_holds_its_slot_against_the_daily_rotation() {
        let lib = library();
        let day = 86_400i64;
        let mut contested = 0;
        for d in 0..366 {
            let now = 1_767_225_600i64 + d * day;
            let open_annual: Vec<uuid::Uuid> = lib
                .iter()
                .filter(|x| x.annual && x.active_instance_start(now).is_some())
                .map(|x| x.quest_id)
                .collect();
            if open_annual.is_empty() {
                continue;
            }
            if uncapped(&lib, now) > MAX_CONCURRENT_EVENTS {
                contested += 1;
            }
            let served: Vec<uuid::Uuid> =
                active_events(&lib, now).iter().map(|e| e.quest_id).collect();
            for q in open_annual {
                assert!(served.contains(&q), "day {d}: annual event {q} was evicted");
            }
        }
        assert!(
            contested > 0,
            "THE CONTROL: if an annual event never competes for a slot this proves nothing"
        );
    }
}
