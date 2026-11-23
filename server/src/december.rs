//! The December 2026 gift calendar (tracker #499), server half: which doors a
//! character is told about, and the rehearsal clock.
//!
//! The calendar itself — the days, their contents, their windows — is
//! [`blades_lib::features::december_2026`]. A door is an ordinary global gift:
//! [`crate::free_for_all::effective_gift`] finds it, the generic claim pays it,
//! and this module only adds the banner that makes its id discoverable.

use std::collections::{HashMap, HashSet};

use blades_lib::features::december_2026;
use blades_lib::static_data::Announcement;
use diesel_async::AsyncPgConnection;
use uuid::Uuid;

/// The most doors advertised to one character at once, oldest first.
///
/// A player who opens the game daily is only ever told about one. A player
/// who returns on the 20th has twenty waiting, and every banner is a dialog:
/// they are shown the five oldest, and the next ones as those are claimed.
pub const MAX_DOORS_ANNOUNCED: usize = 5;

/// `DECEMBER_REHEARSAL_NOW`, the clock a marked TEST character's calendar runs
/// on: unix seconds, or an RFC 3339 time such as `2026-12-06T10:00:00Z`.
///
/// For checking a December day's gift on a device before December. It moves the
/// calendar for test characters (`test_characters`) only; every other character
/// stays on the real clock whatever this says. Unset — the normal state — it is
/// not consulted at all.
pub fn rehearsal_clock() -> Option<i64> {
    static CLOCK: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
    *CLOCK.get_or_init(|| {
        let clock = parse_rehearsal_clock(std::env::var("DECEMBER_REHEARSAL_NOW").ok().as_deref());
        if let Some(at) = clock {
            log::warn!(
                "[december] DECEMBER_REHEARSAL_NOW is set: test characters see the gift calendar at unix {at}"
            );
        }
        clock
    })
}

fn parse_rehearsal_clock(raw: Option<&str>) -> Option<i64> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse::<i64>().ok().filter(|t| *t > 0).or_else(|| {
        chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|t| t.timestamp())
    })
}

/// The clock this character's calendar runs on: the real one, unless a
/// rehearsal is configured AND the character's live alt is a marked test
/// character. A failed lookup is the real clock.
pub async fn clock_for(conn: &mut AsyncPgConnection, character_id: Uuid, now: i64) -> i64 {
    let Some(rehearsal) = rehearsal_clock() else {
        return now;
    };
    match crate::testchar::live_alt_is_test(conn, character_id).await {
        Ok(true) => rehearsal,
        _ => now,
    }
}

/// The banners for the doors this character can open and has not opened yet.
///
/// `calendar_now` is the clock the calendar runs on for this character and
/// `real_now` the wall clock; they differ only for a rehearsing test character.
/// A door whose id is already advertised — an authored `gift_overrides` row
/// replacing it — is not advertised twice.
pub fn door_announcements(
    claims: &HashMap<Uuid, u64>,
    advertised: &HashSet<String>,
    calendar_now: i64,
    real_now: i64,
) -> Vec<Announcement> {
    let rehearsing = calendar_now != real_now;
    december_2026::open_doors(calendar_now)
        .filter(|door| {
            let id = door.gift.global_gift_id;
            claims.get(&id).copied().unwrap_or(0) < door.gift.claim_count_limit
                && !advertised.contains(&id.to_string())
        })
        .take(MAX_DOORS_ANNOUNCED)
        .map(|door| {
            let id = door.gift.global_gift_id;
            // The banner lives exactly as long as the door, so it never arms a
            // Claim that answers "not active". A rehearsed door's own dates are
            // in the future, so its banner is dated around the real clock.
            let (start_time, ttl) = if rehearsing {
                (real_now - 60, real_now + 86_400)
            } else {
                (door.opens_at_secs, door.closes_at_secs)
            };
            Announcement {
                id: id.to_string(),
                r#type: "BASIC".into(),
                start_time,
                ttl,
                // GIFT-BEARING through the edge's `gift/` namespace: the uuid in
                // the path becomes the manifest's `GlobalGiftId` and arms Claim.
                asset_url: format!("https://announcements.blades.bgs.services/gift/{id}"),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use blades_lib::features::december_2026::{DECEMBER_1_SECS, door_gift_id};

    const DAY: i64 = 86_400;

    fn at(day_of_december: u32, hour: i64) -> i64 {
        DECEMBER_1_SECS + (i64::from(day_of_december) - 1) * DAY + hour * 3_600
    }

    fn announced(claimed: &[u32], now: i64) -> Vec<Announcement> {
        let claims: HashMap<Uuid, u64> = claimed.iter().map(|d| (door_gift_id(*d), 1)).collect();
        door_announcements(&claims, &HashSet::new(), now, now)
    }

    fn days(banners: &[Announcement]) -> Vec<u32> {
        banners
            .iter()
            .map(|a| {
                let id = Uuid::parse_str(&a.id).unwrap();
                (1..=31).find(|d| door_gift_id(*d) == id).unwrap()
            })
            .collect()
    }

    /// NOTHING BEFORE 1 DECEMBER 00:00 UTC: no banner on the day this was
    /// written, none in the last second of November — and door 1 a second later.
    #[test]
    fn no_door_is_advertised_before_1_december() {
        for now in [0, DECEMBER_1_SECS - 8 * DAY, DECEMBER_1_SECS - 1] {
            assert!(announced(&[], now).is_empty(), "t={now}");
        }
        assert_eq!(days(&announced(&[], DECEMBER_1_SECS)), vec![1]);
    }

    /// A banner points at its own door's gift, in the gift-bearing namespace,
    /// and lives exactly as long as the door.
    #[test]
    fn a_banner_arms_its_own_door_and_expires_with_it() {
        let banners = announced(&[], at(1, 9));
        let a = &banners[0];
        let id = door_gift_id(1);
        assert_eq!(a.id, id.to_string());
        assert_eq!(
            a.asset_url,
            format!("https://announcements.blades.bgs.services/gift/{id}")
        );
        assert_eq!(a.r#type, "BASIC");
        assert_eq!(a.start_time, DECEMBER_1_SECS);
        assert_eq!(a.ttl, at(32, 0), "doors 1-24 close with the year");

        let eve = announced(&(1..=24).collect::<Vec<_>>(), at(31, 9));
        assert_eq!(days(&eve), vec![31]);
        assert_eq!(eve[0].start_time, at(31, 0));
        assert_eq!(eve[0].ttl, at(33, 0), "New Year's Eve gift: two days");
    }

    /// A player who opens every door sees one banner a day: today's.
    #[test]
    fn a_daily_player_is_told_about_todays_door_only() {
        for day in 1..=24u32 {
            let claimed: Vec<u32> = (1..day).collect();
            assert_eq!(days(&announced(&claimed, at(day, 7))), vec![day]);
            // …and none once it is opened.
            let claimed: Vec<u32> = (1..=day).collect();
            assert!(announced(&claimed, at(day, 8)).is_empty(), "{day} December");
        }
    }

    /// 25-30 December bring no new door: a player up to date sees nothing.
    #[test]
    fn nothing_new_between_christmas_and_new_years_eve() {
        let all: Vec<u32> = (1..=24).collect();
        for day in 25..=30 {
            assert!(announced(&all, at(day, 12)).is_empty(), "{day} December");
        }
    }

    /// A returning player is shown the five oldest unopened doors, then the
    /// next as those are claimed — never the whole backlog at once.
    #[test]
    fn a_backlog_is_advertised_five_at_a_time_oldest_first() {
        assert_eq!(days(&announced(&[], at(20, 12))), vec![1, 2, 3, 4, 5]);
        assert_eq!(
            days(&announced(&[1, 2, 4], at(20, 12))),
            vec![3, 5, 6, 7, 8]
        );
        assert_eq!(MAX_DOORS_ANNOUNCED, 5);
    }

    /// After the calendar: only the New Year's Eve gift on 1 January, and
    /// from 2 January nothing, claimed or not.
    #[test]
    fn nothing_is_advertised_once_the_doors_close() {
        assert_eq!(days(&announced(&[], at(32, 12))), vec![31]);
        assert!(announced(&[], at(33, 0)).is_empty());
        assert!(announced(&[], at(33, 0) + 300 * DAY).is_empty());
    }

    /// A door replaced by an authored gift of the same id is advertised once,
    /// by the authored row.
    #[test]
    fn a_door_already_advertised_is_not_advertised_twice() {
        let taken: HashSet<String> = [door_gift_id(2).to_string()].into();
        let banners = door_announcements(&HashMap::new(), &taken, at(3, 0), at(3, 0));
        assert_eq!(days(&banners), vec![1, 3]);
    }

    /// A rehearsal shows a December day's doors today, dated around today so
    /// the client treats them as live.
    #[test]
    fn a_rehearsed_door_is_dated_around_the_real_clock() {
        let real = DECEMBER_1_SECS - 5 * DAY;
        let banners = door_announcements(&HashMap::new(), &HashSet::new(), at(6, 10), real);
        assert_eq!(days(&banners), vec![1, 2, 3, 4, 5]);
        for a in &banners {
            assert!(a.start_time < real && a.ttl > real);
        }
    }

    /// The rehearsal clock reads unix seconds or an RFC 3339 time, and
    /// anything else — or nothing — is no rehearsal.
    #[test]
    fn the_rehearsal_clock_is_off_unless_it_parses() {
        assert_eq!(parse_rehearsal_clock(None), None);
        assert_eq!(parse_rehearsal_clock(Some("")), None);
        assert_eq!(parse_rehearsal_clock(Some("  ")), None);
        assert_eq!(parse_rehearsal_clock(Some("december")), None);
        assert_eq!(parse_rehearsal_clock(Some("0")), None);
        assert_eq!(parse_rehearsal_clock(Some("1796554800")), Some(at(6, 11)));
        assert_eq!(
            parse_rehearsal_clock(Some("2026-12-06T11:00:00Z")),
            Some(at(6, 11))
        );
        assert_eq!(
            parse_rehearsal_clock(Some("2026-12-06T12:00:00+01:00")),
            Some(at(6, 11))
        );
    }
}
