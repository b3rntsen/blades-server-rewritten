//! Free for All — the recurring "everyone gets gems" giveaway.
//!
//! Retail ran this by hand: on (roughly) the first Saturday of the month every
//! player who turned up in the Arena got 100 Gems, and when Bethesda forgot a
//! month they paid 200 the next time instead. This module is the schedule and
//! the doubling rule as pure arithmetic; the server owns delivery and the web
//! console owns who may press the button.
//!
//! Delivery is the **global-gift** channel, which is the only out-of-band grant
//! the retail client knows how to render. That channel has one hard constraint,
//! and the whole design follows from it: the client only ever asks about gift
//! ids that are **baked into the APK** — in our captures it polls exactly three
//! (`GET .../globalgifts/{id}` for each, 893 times across the corpus) and never
//! discovers a new one. So a run cannot mint a fresh gift id; it re-points an
//! existing one at new contents, a new window, and a claim limit one higher than
//! last time, which is what lets the same id pay out again every month.

use std::time::Duration;

/// Seconds in a day. The window is a whole day because a player should not have
/// to be online at a particular hour to collect.
pub const DAY: i64 = 86_400;

/// What retail paid when it remembered.
pub const BASE_GEMS: u64 = 100;

/// Retail never went past a single doubling in the owner's recollection, and an
/// unbounded catch-up is an economy bug waiting for a long outage: three missed
/// months should not pay 800. Cap the multiplier.
pub const MAX_MULTIPLIER: u32 = 2;

/// How the next occurrence is picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// The first Saturday of each month — what retail appeared to do.
    FirstSaturdayOfMonth,
    /// Every Saturday. Kept because the owner was not certain which it was, and
    /// switching is then a console setting rather than a redeploy.
    EverySaturday,
}

/// Unix seconds → days since the epoch (floor). 1970-01-01 was a Thursday.
fn day_index(t: i64) -> i64 {
    t.div_euclid(DAY)
}

/// 0 = Monday … 5 = Saturday, 6 = Sunday, for a unix timestamp in UTC.
pub fn weekday(t: i64) -> i64 {
    // The epoch fell on a Thursday, which is index 3.
    (day_index(t) + 3).rem_euclid(7)
}

const SATURDAY: i64 = 5;

/// Midnight UTC of the day containing `t`.
pub fn midnight(t: i64) -> i64 {
    day_index(t) * DAY
}

/// Civil date (year, month, day) from a unix timestamp, UTC.
///
/// Hand-rolled rather than pulling `chrono` into `blades_lib`: this crate has no
/// date dependency today and the month boundary is the only calendar fact the
/// schedule needs. Algorithm is Howard Hinnant's `civil_from_days`.
pub fn civil_from_secs(t: i64) -> (i64, u32, u32) {
    let z = day_index(t) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The first occurrence at or after `from`, as midnight UTC of that day.
///
/// `from` is inclusive of the day it falls in: asking on a Saturday morning
/// returns that same Saturday, not the next one, so a run opened late on the day
/// is still the day's run rather than a week early.
pub fn next_occurrence(cadence: Cadence, from: i64) -> i64 {
    let mut day = midnight(from);
    for _ in 0..400 {
        if weekday(day) == SATURDAY {
            match cadence {
                Cadence::EverySaturday => return day,
                Cadence::FirstSaturdayOfMonth => {
                    let (_, _, d) = civil_from_secs(day);
                    // The first Saturday is by definition in the first week.
                    if d <= 7 {
                        return day;
                    }
                }
            }
        }
        day += DAY;
    }
    // Unreachable for any sane cadence; returning `from` keeps this total rather
    // than panicking inside a request handler.
    midnight(from)
}

/// The occurrence strictly after the one containing `after`.
pub fn following_occurrence(cadence: Cadence, after: i64) -> i64 {
    next_occurrence(cadence, midnight(after) + DAY)
}

/// A run that has been opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub opens_at: i64,
    pub closes_at: i64,
    pub gems: u64,
    pub multiplier: u32,
}

/// Plan the next run.
///
/// `last_opened_at` is when the previous run opened (`None` if this is the
/// first). The doubling is derived, never stored as a flag: if at least one
/// scheduled occurrence fell between the last run and this one, the giveaway was
/// missed and this one pays double.
pub fn plan(cadence: Cadence, now: i64, last_opened_at: Option<i64>) -> Run {
    let opens_at = next_occurrence(cadence, now);
    let missed = missed_since(cadence, last_opened_at, opens_at);
    let multiplier = if missed > 0 { MAX_MULTIPLIER } else { 1 };
    Run {
        opens_at,
        closes_at: opens_at + DAY,
        gems: BASE_GEMS * multiplier as u64,
        multiplier,
    }
}

/// How many scheduled occurrences were skipped between the last run and `upto`.
///
/// Zero when there was no previous run: a server that has never run one has not
/// "missed" anything, and opening the very first giveaway at double rate would
/// be a strange debut.
pub fn missed_since(cadence: Cadence, last_opened_at: Option<i64>, upto: i64) -> u32 {
    let Some(last) = last_opened_at else {
        return 0;
    };
    let mut n = 0;
    let mut day = following_occurrence(cadence, last);
    while day < midnight(upto) && n < 100 {
        n += 1;
        day = following_occurrence(cadence, day);
    }
    n
}

/// Whether a planned run is collectable right now.
pub fn is_open(run: &Run, now: i64) -> bool {
    now >= run.opens_at && now < run.closes_at
}

/// How long until a run opens, for the console's countdown.
pub fn time_until(run: &Run, now: i64) -> Option<Duration> {
    (run.opens_at > now).then(|| Duration::from_secs((run.opens_at - now) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-09-18 is a Friday; 2026-09-19 a Saturday. Anchor every case on real
    // dates so a weekday-arithmetic slip fails loudly rather than shifting the
    // whole calendar by one consistent day.
    const FRI_2026_09_18: i64 = 1_789_689_600;

    #[test]
    fn weekday_matches_the_calendar() {
        assert_eq!(weekday(FRI_2026_09_18), 4, "2026-09-18 is a Friday");
        assert_eq!(weekday(FRI_2026_09_18 + DAY), SATURDAY);
        assert_eq!(weekday(0), 3, "the epoch was a Thursday");
    }

    #[test]
    fn civil_date_round_trips_known_days() {
        assert_eq!(civil_from_secs(0), (1970, 1, 1));
        assert_eq!(civil_from_secs(FRI_2026_09_18), (2026, 9, 18));
    }

    #[test]
    fn every_saturday_finds_the_next_saturday() {
        let sat = next_occurrence(Cadence::EverySaturday, FRI_2026_09_18);
        assert_eq!(weekday(sat), SATURDAY);
        assert_eq!(sat, midnight(FRI_2026_09_18) + DAY);
    }

    #[test]
    fn asking_on_the_day_returns_the_same_day() {
        let sat = midnight(FRI_2026_09_18) + DAY;
        // Late on the Saturday itself.
        let found = next_occurrence(Cadence::EverySaturday, sat + 20 * 3600);
        assert_eq!(found, sat, "a run opened late is still that day's run");
    }

    #[test]
    fn first_saturday_lands_in_the_first_week() {
        let mut t = FRI_2026_09_18;
        for _ in 0..14 {
            let day = next_occurrence(Cadence::FirstSaturdayOfMonth, t);
            let (_, _, d) = civil_from_secs(day);
            assert_eq!(weekday(day), SATURDAY);
            assert!(d <= 7, "day-of-month {d} is not in the first week");
            t = day + DAY;
        }
    }

    #[test]
    fn a_month_that_starts_on_saturday_uses_the_first() {
        // 2026-08-01 is a Saturday.
        let jul = next_occurrence(Cadence::FirstSaturdayOfMonth, 1_785_542_400);
        let (y, m, d) = civil_from_secs(jul);
        assert_eq!((y, m), (2026, 8));
        assert_eq!(d, 1);
    }

    #[test]
    fn first_run_is_never_doubled() {
        let run = plan(Cadence::FirstSaturdayOfMonth, FRI_2026_09_18, None);
        assert_eq!(run.multiplier, 1);
        assert_eq!(run.gems, BASE_GEMS);
    }

    #[test]
    fn a_run_on_schedule_pays_the_base_rate() {
        let first = plan(Cadence::FirstSaturdayOfMonth, FRI_2026_09_18, None);
        // Next month, having run last month.
        let later = first.opens_at + 31 * DAY;
        let second = plan(Cadence::FirstSaturdayOfMonth, later, Some(first.opens_at));
        assert_ne!(second.opens_at, first.opens_at, "a new occurrence");
        assert_eq!(second.multiplier, 1, "nothing was skipped");
        assert_eq!(second.gems, BASE_GEMS);
    }

    #[test]
    fn a_skipped_month_doubles_the_next_one() {
        let first = plan(Cadence::FirstSaturdayOfMonth, FRI_2026_09_18, None);
        // Two months later: one occurrence went by unserved.
        let later = first.opens_at + 62 * DAY;
        let second = plan(Cadence::FirstSaturdayOfMonth, later, Some(first.opens_at));
        assert_eq!(second.multiplier, MAX_MULTIPLIER);
        assert_eq!(second.gems, BASE_GEMS * 2);
    }

    #[test]
    fn a_long_outage_still_only_doubles() {
        let first = plan(Cadence::FirstSaturdayOfMonth, FRI_2026_09_18, None);
        let much_later = first.opens_at + 400 * DAY;
        let run = plan(
            Cadence::FirstSaturdayOfMonth,
            much_later,
            Some(first.opens_at),
        );
        assert!(
            missed_since(
                Cadence::FirstSaturdayOfMonth,
                Some(first.opens_at),
                run.opens_at
            ) > 2
        );
        assert_eq!(
            run.gems,
            BASE_GEMS * MAX_MULTIPLIER as u64,
            "capped, not compounded"
        );
    }

    #[test]
    fn the_window_is_a_whole_day() {
        let run = plan(Cadence::EverySaturday, FRI_2026_09_18, None);
        assert_eq!(run.closes_at - run.opens_at, DAY);
        assert!(!is_open(&run, run.opens_at - 1));
        assert!(is_open(&run, run.opens_at));
        assert!(is_open(&run, run.closes_at - 1));
        assert!(
            !is_open(&run, run.closes_at),
            "closed at the boundary, not after"
        );
    }

    #[test]
    fn countdown_is_none_once_open() {
        let run = plan(Cadence::EverySaturday, FRI_2026_09_18, None);
        assert!(time_until(&run, FRI_2026_09_18).is_some());
        assert!(time_until(&run, run.opens_at).is_none());
    }
}
