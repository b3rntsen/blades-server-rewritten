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

impl EventDef {
    /// One of retail's day-slot rotation events: repeats on a day count, and is
    /// neither a holiday (`annual`) nor a one-shot `preview`.
    fn is_rotating(&self) -> bool {
        !self.annual && !self.preview && self.period_secs() > 0
    }
}

/// How long one opening of the rotation stays open: retail's `durationSecs` in all
/// 105 captured sightings. A themed window hands out openings of exactly this
/// length so its days read like any other rotation day.
pub const ROTATION_WINDOW_SECS: i64 = 2 * SECS_PER_DAY;

/// The Halloween set: the events whose own loc strings are witch, undead, ghost or
/// necromancer themed. Keyed by `questId` (the gldQuestId) as `game_events.json` is.
///
/// * EQ40 *Season of the Witch* — "unholy rituals and curses from beyond the grave"
/// * EQ01 *The Spectral Forest* — "dark and cursed woodlands"
/// * EQ16 *Wrath of the Undying* — enemies sustained "in unnatural ways"
/// * EQ21 *The Lich's Tower* — "the cursed consequence of the Sorcerer-King's spells"
/// * EQ24 *Death's Shadow* — "Necromancers … utterly corrupted by the evil spirits"
///
/// EQ42 *The Long Night* is deliberately NOT here: its "powerful ogre and his
/// diminutive helpers sneaking into dwellings at night to reward the faithful and
/// punish the wicked" is midwinter, and it keeps its own December window.
pub const HALLOWEEN_THEME_QUESTS: [Uuid; 5] = [
    uuid::uuid!("f116b952-9932-4b1a-b20d-246ca2945bb4"), // EQ40 Season of the Witch
    uuid::uuid!("7f0d1508-312b-4036-970f-ff5f4c342526"), // EQ01 The Spectral Forest
    uuid::uuid!("26eb6ab5-2d8c-4993-820e-ada79f6f00a8"), // EQ16 Wrath of the Undying
    uuid::uuid!("ff662bf7-a7fa-4ba7-9107-a8556db391e1"), // EQ21 The Lich's Tower
    uuid::uuid!("816ff4c8-b56f-4645-bd2a-29bc7c1baf96"), // EQ24 Death's Shadow
];

/// A themed window: from `start_secs` to `end_secs` (UTC), the day-slot rotation
/// is re-drawn so every themed event opens repeatedly instead of once a cycle.
///
/// NOT retail's schedule — retail's rotation never changed, it is ours, the same
/// way `annual` and `preview` are. What it keeps from retail is the SHAPE, because
/// that is what the client depends on: exactly one opening per UTC day, each open
/// [`ROTATION_WINDOW_SECS`], so two are open and one is about to open at every
/// moment, inside the window, outside it, and across both edges.
///
/// Inside the window every `cadence_days`-th day's opening is a themed event —
/// the least-run one so far, in list order — and the other days walk the ordinary
/// rotation in its usual order with the themed events left out. The cadence is
/// the largest that still gives each themed event two openings
/// (`days / (2 x themed)`, at least 1), so a 21-day window with the five Halloween
/// events opens a themed event every second day and each of them twice or more.
///
/// A themed event's own `annual`/`preview` instance is hidden while the window's
/// openings are live, so it is never open twice at once, and shows as usual on
/// either side. Every other holiday window is left alone, and the per-response
/// caps still apply on top.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventTheme {
    pub start_secs: i64,
    pub end_secs: i64,
    /// `questId`s (or `eventId`s) of the themed events, in rotation order.
    pub quest_ids: Vec<Uuid>,
}

/// `ceil(a / b)` for a positive `b`, correct for negative `a` too.
fn ceil_div(a: i64, b: i64) -> i64 {
    a.div_euclid(b) + i64::from(a.rem_euclid(b) != 0)
}

/// The longest themed window accepted, in days. A season is weeks; a value far
/// beyond this is a typo (milliseconds for seconds), and the plan is rebuilt on
/// every request, so it is refused rather than laid out.
pub const MAX_THEME_DAYS: i64 = 366;

/// The quests of the rotation events whose instance begins on UTC day `day`.
fn baseline_openers(library: &[EventDef], day: i64) -> impl Iterator<Item = Uuid> + '_ {
    library.iter().filter(|d| d.is_rotating()).filter_map(move |d| {
        let start = d.instance_start_at_or_before(day * SECS_PER_DAY + SECS_PER_DAY - 1)?;
        (start.div_euclid(SECS_PER_DAY) == day).then_some(d.quest_id)
    })
}

impl EventTheme {
    /// The first UTC day whose opening falls inside the window.
    fn first_day(&self) -> i64 {
        ceil_div(self.start_secs, SECS_PER_DAY)
    }

    /// The first UTC day after the window (exclusive).
    fn end_day(&self) -> i64 {
        ceil_div(self.end_secs, SECS_PER_DAY)
    }

    /// How many openings (UTC midnights) the window holds.
    pub fn window_days(&self) -> i64 {
        self.end_day().saturating_sub(self.first_day())
    }

    /// Lay out the window's openings, one per day. `None` when the window is
    /// empty or none of its events is in the library — then nothing changes.
    pub fn plan<'a>(&self, library: &'a [EventDef]) -> Option<ThemePlan<'a>> {
        let first_day = self.first_day();
        let end_day = self.end_day();
        if end_day <= first_day || end_day - first_day > MAX_THEME_DAYS {
            return None;
        }
        // Resolve each id to one definition, preferring its rotation entry.
        let mut themed: Vec<&EventDef> = Vec::new();
        for id in &self.quest_ids {
            let matches = |d: &&EventDef| d.quest_id == *id || d.event_id == *id;
            let def = library
                .iter()
                .filter(matches)
                .find(|d| d.is_rotating())
                .or_else(|| library.iter().find(matches));
            if let Some(def) = def {
                if !themed.iter().any(|t| t.quest_id == def.quest_id) {
                    themed.push(def);
                }
            }
        }
        if themed.is_empty() {
            return None;
        }
        let themed_quests: Vec<Uuid> = themed.iter().map(|d| d.quest_id).collect();

        // The ordinary rotation, in the order it would have run from day one of
        // the window, with the themed events taken out.
        let window_start = first_day * SECS_PER_DAY;
        let mut walk: Vec<(i64, &EventDef)> = library
            .iter()
            .filter(|d| d.is_rotating() && !themed_quests.contains(&d.quest_id))
            .filter_map(|d| {
                let at = match d.instance_start_at_or_before(window_start) {
                    Some(s) if s == window_start => s,
                    _ => d.next_instance_start_after(window_start)?,
                };
                Some((at, d))
            })
            .collect();
        walk.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.event_id.cmp(&b.1.event_id)));
        let walk: Vec<&EventDef> = walk.into_iter().map(|(_, d)| d).collect();

        let days = end_day - first_day;
        let cadence_days = (days / (2 * themed.len() as i64)).max(1);

        let mut openers: Vec<(&EventDef, bool)> = Vec::with_capacity(days as usize);
        let mut themed_runs = vec![0u32; themed.len()];
        let mut walk_runs = vec![0u32; walk.len()];
        for i in 0..days {
            let day = first_day + i;
            // Never the same quest as an opening within two days either side:
            // those are the ones open, or announced, at the same time as this.
            let mut taken: Vec<Uuid> = Vec::new();
            for d in [day - 2, day - 1] {
                if d >= first_day {
                    taken.push(openers[(d - first_day) as usize].0.quest_id);
                } else {
                    taken.extend(baseline_openers(library, d));
                }
            }
            for d in [day + 1, day + 2] {
                if d >= end_day {
                    taken.extend(baseline_openers(library, d));
                }
            }
            // …nor a holiday or preview instance that is visible while this
            // opening is announced or open. A themed one is hidden only while
            // the window's openings are live, so at the window's edges it can
            // still be on screen next to this one.
            let (lo, hi) = ((day - 1) * SECS_PER_DAY, (day + 3) * SECS_PER_DAY);
            let hidden = hidden_span(first_day, end_day);
            for d in library.iter().filter(|d| !d.is_rotating()) {
                let Some(s) = d.instance_start_at_or_before(hi - 1) else {
                    continue;
                };
                let e = s + d.window_secs();
                let visible: Vec<(i64, i64)> = if themed_quests.contains(&d.quest_id) {
                    vec![(s, e.min(hidden.0)), (s.max(hidden.1), e)]
                } else {
                    vec![(s, e)]
                };
                if visible.iter().any(|&(a, b)| a < b && a < hi && b > lo) {
                    taken.push(d.quest_id);
                }
            }
            // The least-run candidate that is free, earliest in order on a tie.
            let pick = |cands: &[&EventDef], runs: &[u32], strict: bool| {
                (0..cands.len())
                    .filter(|&j| !strict || !taken.contains(&cands[j].quest_id))
                    .min_by_key(|&j| (runs[j], j))
            };
            let chosen = (i % cadence_days == 0)
                .then(|| pick(&themed, &themed_runs, true).map(|j| (true, j)))
                .flatten()
                .or_else(|| pick(&walk, &walk_runs, true).map(|j| (false, j)))
                .or_else(|| pick(&walk, &walk_runs, false).map(|j| (false, j)))
                .or_else(|| pick(&themed, &themed_runs, false).map(|j| (true, j)));
            let Some((is_themed, j)) = chosen else {
                return None; // unreachable: `themed` is non-empty
            };
            if is_themed {
                themed_runs[j] += 1;
                openers.push((themed[j], true));
            } else {
                walk_runs[j] += 1;
                openers.push((walk[j], false));
            }
        }
        Some(ThemePlan {
            first_day,
            openers,
            themed_quests,
            cadence_days,
        })
    }
}

/// A themed window laid out against a library: which event opens on each day.
#[derive(Debug)]
pub struct ThemePlan<'a> {
    first_day: i64,
    openers: Vec<(&'a EventDef, bool)>,
    themed_quests: Vec<Uuid>,
    cadence_days: i64,
}

impl<'a> ThemePlan<'a> {
    /// Every how many days a themed event opens.
    pub fn cadence_days(&self) -> i64 {
        self.cadence_days
    }

    /// The themed events that resolved against the library, in rotation order.
    pub fn themed_quest_ids(&self) -> &[Uuid] {
        &self.themed_quests
    }

    /// `(opening start, questId, themed?)` for every day of the window.
    pub fn openings(&self) -> impl Iterator<Item = (i64, Uuid, bool)> + '_ {
        self.openers.iter().enumerate().map(|(i, (d, t))| {
            ((self.first_day + i as i64) * SECS_PER_DAY, d.quest_id, *t)
        })
    }

    fn end_day(&self) -> i64 {
        self.first_day + self.openers.len() as i64
    }

    /// The window's opening on UTC day `day`, if `day` is one of its days.
    fn opening(&self, day: i64) -> Option<Scheduled<'a>> {
        if day < self.first_day || day >= self.end_day() {
            return None;
        }
        let (def, themed) = self.openers[(day - self.first_day) as usize];
        let start = day * SECS_PER_DAY;
        Some(Scheduled {
            start,
            def,
            window: ROTATION_WINDOW_SECS,
            // Describe the series this instance really belongs to, rather than
            // the def's own calendar, which it does not follow inside the window.
            recurrence: Recurrence {
                recurrence_type: def.recurrence.recurrence_type.clone(),
                start_time_secs: start,
                duration_secs: ROTATION_WINDOW_SECS,
                recurrence_interval: if themed {
                    self.cadence_days * self.themed_quests.len() as i64
                } else {
                    def.recurrence.recurrence_interval
                },
            },
        })
    }

    /// Whether the window hides this def's own instance beginning at `start`,
    /// at the moment `at` it would be shown (now, or its start for a warning).
    ///
    /// Every rotation instance that would have opened on a window day is replaced
    /// outright (the window hands that day out itself). A themed event's holiday
    /// or preview instance is hidden only while the window's openings are live,
    /// so the same quest is never open twice, and it shows as usual either side
    /// of that. Anything else is untouched.
    fn displaces(&self, def: &EventDef, start: i64, at: i64) -> bool {
        if def.is_rotating() {
            let day = start.div_euclid(SECS_PER_DAY);
            return day >= self.first_day && day < self.end_day();
        }
        if self.themed_quests.contains(&def.quest_id) {
            let (from, to) = hidden_span(self.first_day, self.end_day());
            return from <= at && at < to;
        }
        false
    }
}

/// When a themed window's openings can be live: from its first opening until
/// the last one closes (it opens the day before `end_day`, for two days).
fn hidden_span(first_day: i64, end_day: i64) -> (i64, i64) {
    (
        first_day * SECS_PER_DAY,
        (end_day - 1) * SECS_PER_DAY + ROTATION_WINDOW_SECS,
    )
}

/// One instance chosen to be on the wire, with the window it runs for.
struct Scheduled<'a> {
    start: i64,
    def: &'a EventDef,
    window: i64,
    recurrence: Recurrence,
}

impl<'a> Scheduled<'a> {
    /// The def's own instance, exactly as [`EventDef::instance`] describes it.
    fn natural(def: &'a EventDef, start: i64) -> Self {
        Scheduled {
            start,
            def,
            window: def.window_secs(),
            recurrence: def.recurrence.clone(),
        }
    }

    fn event(&self) -> GameEvent {
        GameEvent {
            game_event_instance_id: format!("{}::{}", self.def.event_id, self.start),
            r#type: "quest".to_string(),
            start_time_secs: self.start,
            end_time_secs: self.start + self.window,
            recurrence: self.recurrence.clone(),
            quest_id: self.def.quest_id,
            important: self.def.important,
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
///
/// This is [`open_instances_themed`] with no themed window. Production code goes
/// through the themed form so a configured window reaches every caller alike.
pub fn open_instances<'a>(library: &'a [EventDef], now_secs: i64) -> Vec<(i64, &'a EventDef)> {
    open_instances_themed(library, None, now_secs)
}

/// [`open_instances`] with an optional [`EventTheme`] applied. `None`, or a theme
/// whose window is nowhere near `now`, gives exactly the untouched calendar.
pub fn open_instances_themed<'a>(
    library: &'a [EventDef],
    theme: Option<&EventTheme>,
    now_secs: i64,
) -> Vec<(i64, &'a EventDef)> {
    open_scheduled(library, theme, now_secs)
        .into_iter()
        .map(|s| (s.start, s.def))
        .collect()
}

fn open_scheduled<'a>(
    library: &'a [EventDef],
    theme: Option<&EventTheme>,
    now_secs: i64,
) -> Vec<Scheduled<'a>> {
    let plan = theme.and_then(|t| t.plan(library));
    let mut out: Vec<Scheduled> = library
        .iter()
        .filter_map(|def| {
            let start = def.active_instance_start(now_secs)?;
            if plan.as_ref().is_some_and(|p| p.displaces(def, start, now_secs)) {
                return None;
            }
            Some(Scheduled::natural(def, start))
        })
        .collect();
    if let Some(plan) = &plan {
        // A rotation opening lasts two days, so only today's and yesterday's can
        // cover `now`.
        let today = now_secs.div_euclid(SECS_PER_DAY);
        for day in [today - 1, today] {
            if let Some(s) = plan.opening(day) {
                if s.start <= now_secs && now_secs < s.start + s.window {
                    out.push(s);
                }
            }
        }
    }
    if out.len() > MAX_CONCURRENT_EVENTS {
        // Annual first, then most recently opened; `event_id` breaks ties so the
        // choice is stable rather than dependent on library order.
        out.sort_by(|a, b| {
            b.def
                .annual
                .cmp(&a.def.annual)
                .then_with(|| b.start.cmp(&a.start))
                .then_with(|| a.def.event_id.cmp(&b.def.event_id))
        });
        out.truncate(MAX_CONCURRENT_EVENTS);
    }
    // The wire is ordered by start, whatever the selection was.
    out.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.def.event_id.cmp(&b.def.event_id)));
    out
}

pub fn active_events(library: &[EventDef], now_secs: i64) -> Vec<GameEvent> {
    active_events_themed(library, None, now_secs)
}

pub fn active_events_themed(
    library: &[EventDef],
    theme: Option<&EventTheme>,
    now_secs: i64,
) -> Vec<GameEvent> {
    open_scheduled(library, theme, now_secs)
        .iter()
        .map(Scheduled::event)
        .collect()
}

/// The events whose next instance opens within `lead_secs` of `now` — retail's
/// `gameEventQuestsInWarning`, i.e. "starting soon", not "ending soon".
/// How many events retail ever announces as "starting soon" at once: one.
///
/// MEASURED over 773 captured `/quests` bodies: `gameEventQuestsInWarning` holds
/// exactly 1 entry in 686 of them and 0 in the other 87. It is never 2.
///
/// This is the sibling of [`MAX_CONCURRENT_EVENTS`] and exists for the same
/// reason. The open-event array had no ceiling either until a third open event
/// left the whole quest map spinning — the client renders the screen from the
/// boot `/quests` response and never re-requests it, so an array one longer
/// than retail ever sends wedges the screen until the app is restarted. The
/// warning array feeds the same screen and had the same shape of hole.
pub const MAX_WARNING_EVENTS: usize = 1;

pub fn upcoming_events(library: &[EventDef], now_secs: i64, lead_secs: i64) -> Vec<GameEvent> {
    upcoming_events_themed(library, None, now_secs, lead_secs)
}

pub fn upcoming_events_themed(
    library: &[EventDef],
    theme: Option<&EventTheme>,
    now_secs: i64,
    lead_secs: i64,
) -> Vec<GameEvent> {
    let plan = theme.and_then(|t| t.plan(library));
    let mut out: Vec<GameEvent> = library
        .iter()
        .filter_map(|def| {
            let start = def.next_instance_start_after(now_secs)?;
            if plan.as_ref().is_some_and(|p| p.displaces(def, start, start)) {
                return None;
            }
            (start - now_secs <= lead_secs).then(|| def.instance(start))
        })
        .collect();
    if let Some(plan) = &plan {
        let today = now_secs.div_euclid(SECS_PER_DAY);
        for day in (today + 1)..=(today + 1 + lead_secs.max(0) / SECS_PER_DAY) {
            if let Some(s) = plan.opening(day) {
                if s.start > now_secs && s.start - now_secs <= lead_secs {
                    out.push(s.event());
                }
            }
        }
    }
    out.sort_by_key(|e| (e.start_time_secs, e.game_event_instance_id.clone()));
    // Keep the soonest. A warning is an announcement of the next event, and the
    // sort above is by start time, so the head is the one retail would name.
    out.truncate(MAX_WARNING_EVENTS);
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

    /// The warning array has a ceiling too, and it is 1.
    ///
    /// MEASURED: `gameEventQuestsInWarning` holds exactly 1 entry in 686 of 773
    /// captured `/quests` bodies and 0 in the other 87. Never 2.
    ///
    /// Swept across a year against a library dense enough that several events
    /// start inside the same 24 h lead window. Without the truncate this returns
    /// 2 and 3 — a shape retail never sent, on the same screen whose other array
    /// wedged the quest map when it ran one entry long.
    #[test]
    fn at_most_one_event_is_ever_announced_as_starting_soon() {
        const DAY: i64 = 86_400;
        let lib: Vec<EventDef> = (0..5u128)
            .map(|i| {
                let mut d = def(i);
                d.recurrence.recurrence_type = "daily".to_string();
                d.recurrence.recurrence_interval = 1;
                d.recurrence.start_time_secs = 1663214400 + (i as i64) * 3 * 3600;
                d
            })
            .collect();

        let mut saw_a_warning = false;
        let mut cap_was_exercised = false;
        for day in 0..365i64 {
            let now = 1663214400 + day * DAY;
            let got = upcoming_events(&lib, now, DAY);
            assert!(
                got.len() <= MAX_WARNING_EVENTS,
                "day {day}: announced {} events as starting soon",
                got.len()
            );
            if !got.is_empty() {
                saw_a_warning = true;
            }
            // Control: count what the window actually holds, so a bug that just
            // empties the array cannot make this pass by vacuity.
            let in_window = lib
                .iter()
                .filter(|d| {
                    d.next_instance_start_after(now)
                        .is_some_and(|start| start - now <= DAY)
                })
                .count();
            if in_window > MAX_WARNING_EVENTS {
                cap_was_exercised = true;
                assert_eq!(got.len(), 1, "day {day}: the warning was dropped entirely");
            }
        }
        assert!(saw_a_warning, "the sweep never produced a warning at all");
        assert!(
            cap_was_exercised,
            "the fixture never crowded the window, so the cap was never tested"
        );
    }

    /// And it keeps the SOONEST one, which is what an announcement means.
    #[test]
    fn the_announced_event_is_the_next_one_to_start() {
        const DAY: i64 = 86_400;
        let mut early = def(1);
        early.recurrence.recurrence_type = "daily".to_string();
        early.recurrence.recurrence_interval = 1;
        let mut late = def(2);
        late.recurrence.recurrence_type = "daily".to_string();
        late.recurrence.recurrence_interval = 1;
        late.recurrence.start_time_secs = early.recurrence.start_time_secs + 6 * 3600;

        let now = early.recurrence.start_time_secs + DAY - 3600;
        let soonest = [&early, &late]
            .iter()
            .filter_map(|d| d.next_instance_start_after(now))
            .min()
            .expect("something starts next");

        let got = upcoming_events(&[early, late], now, DAY);
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].start_time_secs, soonest,
            "the warning named a later event than the one starting next"
        );
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

/// Report: "make sure that the themed quests get run more than once while theme
/// is on." A themed window re-draws the rotation for its days, and must change
/// nothing else.
///
/// Everything here runs against the COMMITTED calendar (`game_events.json`,
/// holiday entries included), because a themed window only means anything next
/// to the real rotation it interleaves with.
#[cfg(test)]
mod themed_window {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    const DAY: i64 = SECS_PER_DAY;
    const HOUR: i64 = 3_600;

    fn library() -> Vec<EventDef> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/game_events.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        serde_json::from_str(&raw).expect("valid game_events.json")
    }

    /// Three weeks of Halloween 2026: 10 Oct 00:00 UTC to 31 Oct 00:00 UTC.
    fn three_weeks() -> EventTheme {
        EventTheme {
            start_secs: utc_midnight(2026, 10, 10),
            end_secs: utc_midnight(2026, 10, 31),
            quest_ids: HALLOWEEN_THEME_QUESTS.to_vec(),
        }
    }

    /// What `/gameevents` carries: the open events, then the next to open.
    fn feed(lib: &[EventDef], theme: Option<&EventTheme>, now: i64) -> Vec<GameEvent> {
        let mut out = active_events_themed(lib, theme, now);
        out.extend(upcoming_events_themed(lib, theme, now, WARNING_LEAD_SECS));
        out
    }

    fn json(events: &[GameEvent]) -> String {
        serde_json::to_string(events).unwrap()
    }

    /// Three weeks with Halloween in the middle: 17 Oct to 7 Nov 2026, which
    /// swallows Season of the Witch's own 24 Oct - 1 Nov holiday instance.
    fn around_halloween() -> EventTheme {
        EventTheme {
            start_secs: utc_midnight(2026, 10, 17),
            end_secs: utc_midnight(2026, 11, 7),
            quest_ids: HALLOWEEN_THEME_QUESTS.to_vec(),
        }
    }

    /// Three weeks, all of them 21 days, placed against the holiday instance:
    /// before it, over its start, around it, and over its end.
    fn windows() -> [EventTheme; 4] {
        let w = |(m0, d0), (m1, d1)| EventTheme {
            start_secs: utc_midnight(2026, m0, d0),
            end_secs: utc_midnight(2026, m1, d1),
            quest_ids: HALLOWEEN_THEME_QUESTS.to_vec(),
        };
        [
            w((10, 2), (10, 23)),  // ends the day before the holiday opens
            three_weeks(),         // 10-31 Oct: over its start
            around_halloween(),    // 17 Oct - 7 Nov: all of it
            w((10, 30), (11, 20)), // starts in the holiday's last days
        ]
    }

    #[test]
    fn every_halloween_event_resolves_against_the_committed_calendar() {
        let lib = library();
        let plan = three_weeks().plan(&lib).expect("the window plans");
        assert_eq!(
            plan.themed_quest_ids(),
            &HALLOWEEN_THEME_QUESTS[..],
            "a themed id no longer matches any event in game_events.json"
        );
        // 21 days / (2 x 5 events) = every second day.
        assert_eq!(plan.cadence_days(), 2);
    }

    /// Outside the window the calendar is byte-for-byte what it is with no theme.
    ///
    /// Swept hourly across two years, skipping only the window's days and the two
    /// either side of it (whose feed shows the window's first or last openings).
    /// The CONTROL proves the theme is actually live in this fixture: without it,
    /// "unchanged everywhere" would pass by the theme doing nothing.
    #[test]
    fn outside_the_window_the_rotation_is_exactly_as_today() {
        for theme in windows() {
            unchanged_outside(&theme);
        }
    }

    fn unchanged_outside(theme: &EventTheme) {
        let lib = library();
        let theme = theme.clone();
        let (first, end) = (theme.start_secs / DAY, theme.end_secs / DAY);
        let from = utc_midnight(2026, 1, 1);
        let mut compared = 0;
        let mut differed_inside = 0;
        let mut t = from;
        while t < from + 2 * 365 * DAY {
            let day = t.div_euclid(DAY);
            let near = day >= first - 2 && day < end + 2;
            let themed = feed(&lib, Some(&theme), t);
            let plain = feed(&lib, None, t);
            if near {
                if json(&themed) != json(&plain) {
                    differed_inside += 1;
                }
            } else {
                assert_eq!(json(&themed), json(&plain), "t={t}: changed outside the window");
                assert_eq!(
                    open_instances_themed(&lib, Some(&theme), t)
                        .iter()
                        .map(|(s, d)| (*s, d.event_id))
                        .collect::<Vec<_>>(),
                    open_instances(&lib, t)
                        .iter()
                        .map(|(s, d)| (*s, d.event_id))
                        .collect::<Vec<_>>(),
                    "t={t}: the quest rows would change outside the window"
                );
                compared += 1;
            }
            t += HOUR;
        }
        // Two years of hours less the 25 days skipped; includes October 2027,
        // because a window is one-off and does not come back a year later.
        assert_eq!(compared, (2 * 365 - 25) * 24, "hours compared");
        assert!(differed_inside > 0, "THE CONTROL: the theme never changed the feed");

        // And a theme with no events it can resolve is no theme at all.
        let empty = EventTheme {
            quest_ids: vec![Uuid::from_u128(1)],
            ..three_weeks()
        };
        for h in 0..(30 * 24) {
            let t = theme.start_secs - 5 * DAY + h * HOUR;
            assert_eq!(json(&feed(&lib, Some(&empty), t)), json(&feed(&lib, None, t)));
        }
    }

    /// The reviewer's case, pinned: a window that ENDS the day before Season of
    /// the Witch's holiday opens must leave that holiday alone — the witch is
    /// served on Halloween, on its own 24 October instance, as with no theme.
    #[test]
    fn a_window_that_ends_before_the_holiday_leaves_it_alone() {
        let lib = library();
        let witch = HALLOWEEN_THEME_QUESTS[0];
        let before = &windows()[0];
        let halloween = utc_midnight(2026, 10, 31) + 12 * HOUR;
        let served = |theme: Option<&EventTheme>| {
            active_events_themed(&lib, theme, halloween)
                .into_iter()
                .find(|e| e.quest_id == witch)
                .map(|e| e.game_event_instance_id)
        };
        assert!(served(None).is_some(), "CONTROL: the holiday is open on Halloween");
        assert_eq!(served(Some(before)), served(None));
    }

    /// The point of the change: inside a three-week window every themed event
    /// opens at least twice. Counted from what the feed actually served, not
    /// from the plan, so a plan the feed ignored would fail here.
    #[test]
    fn inside_a_three_week_window_every_themed_event_opens_at_least_twice() {
        for window in windows() {
            themed_events_open_at_least_twice_in(&window);
        }
    }

    fn themed_events_open_at_least_twice_in(window: &EventTheme) {
        let lib = library();
        let theme = window.clone();
        let count = |theme: Option<&EventTheme>| {
            let mut openings: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
            let mut t = window.start_secs;
            while t < window.end_secs {
                for e in active_events_themed(&lib, theme, t) {
                    openings
                        .entry(e.quest_id)
                        .or_default()
                        .insert(e.game_event_instance_id);
                }
                t += HOUR;
            }
            openings
        };

        let themed = count(Some(&theme));
        for q in HALLOWEEN_THEME_QUESTS {
            let n = themed.get(&q).map_or(0, |s| s.len());
            assert!(n >= 2, "themed event {q} opened {n} time(s) in three weeks");
        }

        // CONTROL: the untouched calendar gives each at most one opening in the
        // same three weeks (a 44-day cycle, and EQ40 once, from 24 October), so
        // the assertion above is measuring the window, not the calendar.
        let plain = count(None);
        for q in HALLOWEEN_THEME_QUESTS {
            let n = plain.get(&q).map_or(0, |s| s.len());
            assert!(n <= 1, "without a theme {q} already opened {n} times");
        }
    }

    /// Retail's shape, held every hour from three days before the window to three
    /// days after it: two open, one about to open, never the same quest twice,
    /// and one opening per UTC day, each open two days.
    #[test]
    fn two_open_and_one_upcoming_every_hour_across_the_window_and_its_edges() {
        for window in windows() {
            shape_holds_around(&window);
        }
    }

    fn shape_holds_around(theme: &EventTheme) {
        let lib = library();
        let theme = theme.clone();
        let mut t = theme.start_secs - 3 * DAY;
        while t < theme.end_secs + 3 * DAY {
            let open = active_events_themed(&lib, Some(&theme), t);
            let soon = upcoming_events_themed(&lib, Some(&theme), t, WARNING_LEAD_SECS);
            assert_eq!(open.len(), 2, "t={t}: {} open", open.len());
            assert_eq!(soon.len(), 1, "t={t}: {} about to open", soon.len());

            let quests: BTreeSet<Uuid> = open.iter().chain(&soon).map(|e| e.quest_id).collect();
            assert_eq!(quests.len(), 3, "t={t}: one quest is both open and announced");

            // One opening per day: yesterday's and today's, then tomorrow's.
            // Outside the window a holiday instance may legitimately be on the
            // feed, as it is with no theme (the cap keeps it plus today's).
            let today = t.div_euclid(DAY) * DAY;
            let (rotation, holiday): (Vec<&GameEvent>, Vec<&GameEvent>) = open
                .iter()
                .partition(|e| e.end_time_secs - e.start_time_secs == ROTATION_WINDOW_SECS);
            let starts: Vec<i64> = rotation.iter().map(|e| e.start_time_secs).collect();
            if holiday.is_empty() {
                assert_eq!(starts, vec![today - DAY, today], "t={t}");
            } else {
                let (from, to) = hidden_span(theme.first_day(), theme.end_day());
                assert!(!(from..to).contains(&t), "t={t}: a holiday shown inside the window");
                assert_eq!(starts, vec![today], "t={t}");
            }
            assert_eq!(soon[0].start_time_secs, today + DAY, "t={t}");
            t += HOUR;
        }
    }

    /// This window overlaps Season of the Witch's own 24 Oct - 1 Nov holiday
    /// instance. That instance must give way to the window's openings — else the
    /// witch would be open twice at once — and the window must still hand it
    /// out repeatedly.
    #[test]
    fn a_themed_holiday_instance_gives_way_to_the_window() {
        let lib = library();
        let witch = HALLOWEEN_THEME_QUESTS[0];
        let theme = around_halloween();
        let holiday = lib
            .iter()
            .find(|d| d.quest_id == witch && d.annual)
            .expect("the annual Season of the Witch");
        let halloween = utc_midnight(2026, 10, 31) + HOUR;
        let holiday_start = holiday.instance_start_at_or_before(halloween).unwrap();
        // CONTROL: untouched, the holiday instance is open on Halloween.
        assert!(
            active_events(&lib, halloween)
                .iter()
                .any(|e| e.start_time_secs == holiday_start && e.quest_id == witch)
        );

        let mut witch_openings = BTreeSet::new();
        let mut t = theme.start_secs - 3 * DAY;
        while t < theme.end_secs + 3 * DAY {
            let open = active_events_themed(&lib, Some(&theme), t);
            assert_eq!(open.len(), 2, "t={t}");
            let witches: Vec<_> = open.iter().filter(|e| e.quest_id == witch).collect();
            assert!(witches.len() <= 1, "t={t}: the witch is open twice");
            for e in witches {
                assert_ne!(e.start_time_secs, holiday_start, "t={t}: the 8-day instance survived");
                witch_openings.insert(e.start_time_secs);
            }
            t += HOUR;
        }
        assert!(
            witch_openings.len() >= 2,
            "the witch opened {} time(s)",
            witch_openings.len()
        );
    }

    /// The wire object is the same shape inside the window as outside it — same
    /// keys, same instance-id form, retail's two-day window — so nothing the
    /// client parses can tell a themed day from an ordinary one.
    #[test]
    fn a_themed_instance_has_the_same_wire_shape() {
        let lib = library();
        let theme = three_weeks();
        let now = theme.start_secs + 5 * DAY + HOUR;
        let themed = feed(&lib, Some(&theme), now);
        let plain = feed(&lib, None, now);
        let keys = |e: &GameEvent| {
            let v = serde_json::to_value(e).unwrap();
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.extend(
                v["recurrence"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(|s| format!("recurrence.{s}")),
            );
            k.sort();
            k
        };
        assert_eq!(themed.len(), plain.len());
        for (a, b) in themed.iter().zip(&plain) {
            assert_eq!(keys(a), keys(b));
            let (event_id, start) = a.game_event_instance_id.split_once("::").unwrap();
            assert!(
                lib.iter()
                    .any(|d| d.event_id.to_string() == event_id && d.quest_id == a.quest_id)
            );
            assert_eq!(start.parse::<i64>().unwrap(), a.start_time_secs);
            assert_eq!(a.r#type, "quest");
            assert_eq!(a.recurrence.duration_secs, ROTATION_WINDOW_SECS);
            assert_eq!(a.recurrence.start_time_secs, a.start_time_secs);
        }
        // CONTROL: this moment really is a themed one.
        assert_ne!(json(&themed), json(&plain));
    }

    /// Deterministic: the schedule does not depend on library order, and a day's
    /// opening does not depend on when during the day it is asked for.
    #[test]
    fn the_schedule_is_deterministic() {
        let lib = library();
        let theme = three_weeks();
        let a: Vec<_> = theme.plan(&lib).unwrap().openings().collect();
        // The order the library happens to be in must not matter.
        let reversed: Vec<EventDef> = lib.iter().rev().cloned().collect();
        let b: Vec<_> = theme.plan(&reversed).unwrap().openings().collect();
        assert_eq!(a, b, "the schedule depends on library order");
        assert_eq!(a.len(), 21, "one opening per day of the window");
        // Themed days are every `cadence` days, starting with the first.
        for (i, (_, _, themed)) in a.iter().enumerate() {
            assert_eq!(*themed, i % 2 == 0, "day {i}");
        }
        for (start, quest, _) in &a {
            for h in [0, 7, 23] {
                let open = active_events_themed(&lib, Some(&theme), start + h * HOUR);
                assert!(
                    open.iter()
                        .any(|e| e.start_time_secs == *start && e.quest_id == *quest),
                    "day starting {start}, hour {h}"
                );
            }
        }
    }

    /// Bad bounds are refused, not laid out: a window in milliseconds would be
    /// ~20 million days rebuilt on every request, and `i64::MIN` must not panic.
    #[test]
    fn absurd_windows_are_refused() {
        let lib = library();
        let ms = EventTheme {
            start_secs: utc_midnight(2026, 10, 10) * 1000,
            end_secs: utc_midnight(2026, 10, 31) * 1000,
            ..three_weeks()
        };
        assert!(ms.window_days() > MAX_THEME_DAYS);
        assert!(ms.plan(&lib).is_none());
        let extreme = EventTheme {
            start_secs: i64::MIN,
            end_secs: i64::MAX,
            ..three_weeks()
        };
        assert!(extreme.plan(&lib).is_none());
        assert_eq!(ceil_div(-1, DAY), 0);
        assert_eq!(ceil_div(1, DAY), 1);
        assert_eq!(ceil_div(DAY, DAY), 1);
        assert_eq!(ceil_div(-DAY - 1, DAY), -1);
    }

    /// A single themed event over a short window still never overlaps itself.
    #[test]
    fn a_single_themed_event_never_overlaps_itself() {
        let lib = library();
        let theme = EventTheme {
            start_secs: utc_midnight(2026, 10, 24),
            end_secs: utc_midnight(2026, 11, 1),
            quest_ids: vec![HALLOWEEN_THEME_QUESTS[0]],
        };
        let plan = theme.plan(&lib).unwrap();
        assert_eq!(plan.cadence_days(), 4, "8 days / (2 x 1)");
        let mut t = theme.start_secs - 3 * DAY;
        while t < theme.end_secs + 3 * DAY {
            let open = active_events_themed(&lib, Some(&theme), t);
            let soon = upcoming_events_themed(&lib, Some(&theme), t, WARNING_LEAD_SECS);
            let quests: BTreeSet<Uuid> = open.iter().chain(&soon).map(|e| e.quest_id).collect();
            assert_eq!(quests.len(), open.len() + soon.len(), "t={t}");
            assert_eq!(open.len(), 2, "t={t}");
            assert_eq!(soon.len(), 1, "t={t}");
            t += HOUR;
        }
        let witch_days = plan
            .openings()
            .filter(|(_, q, _)| *q == HALLOWEEN_THEME_QUESTS[0])
            .count();
        assert_eq!(witch_days, 2, "8 days at a 4-day cadence");
    }
}
