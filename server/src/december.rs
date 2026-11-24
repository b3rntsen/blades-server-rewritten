//! The December 2026 gift calendar (tracker #499), server half: which
//! character of an account holds the day's door, which door a character is
//! told about, and the rehearsal clock.
//!
//! The calendar itself — the days, their contents, their windows — is
//! [`blades_lib::features::december_2026`]. A door is an ordinary global gift:
//! [`crate::free_for_all::effective_gift`] finds it, the generic claim pays it,
//! and this module adds the banner that makes its id discoverable and the
//! one-door-per-account rule.
//!
//! # The first character of the day
//!
//! HauDrauf, 2026-11-24 (message 3924): the day's door goes only to the
//! character of an account that logs in first that day. What the server can
//! observe of a "login" is an authenticated request for one character, so:
//!
//! **The door of day N belongs to the alt that is live on the account's
//! character row at the first `/announcements` request or door claim the server
//! answers for that account on day N (UTC).**
//!
//! * `/announcements` is the request the client makes for the character it has
//!   just loaded — before it can show any banner — so it is the earliest
//!   reliable sign that a character is being played. Binding there means the
//!   holder is decided before anyone is offered the door, and an alt that is not
//!   the holder is never shown it.
//! * The claim binds as well, so a client that somehow skips `/announcements`
//!   cannot take the door without being first either.
//! * The account is the arena user id: alts share it (one `characters` row per
//!   user, `uq_characters_user_id`) and differ by `source_alt_uuid`.
//! * The binding is one `INSERT … ON CONFLICT DO NOTHING` on (account, day): of
//!   two requests racing at midnight exactly one writes the row, and the other
//!   reads it back afterwards. At the claim it runs inside the transaction that
//!   holds the character row, so the live alt cannot switch in between.
//! * Switching alts later that day changes nothing: the door stays with the
//!   holder, who can still open it on returning to that alt that day. A
//!   character created that day is a character like any other: if it is the
//!   first one loaded, it holds the door.
//! * The armour a door could not fit is on the holder's own `server_state`, so
//!   its payout from 25 December follows that character and nobody else.

use std::collections::{HashMap, HashSet};

use blades_lib::features::december_2026;
use blades_lib::static_data::Announcement;
use diesel::sql_types::{Bool, Int2, Int8, Uuid as SqlUuid};
use diesel::{OptionalExtension, QueryableByName};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use uuid::Uuid;

/// Whether the character playing `character_id` holds the door of `day` for
/// its account, binding it to the door if the account has no holder for that
/// day yet. `rehearsal` keeps a rehearsing test character's binding apart from
/// the real day's (see the migration `december_2026_door_holders`).
///
/// `false` for a character row that does not exist. An error is the caller's
/// to treat as "not the holder": never offer or pay a door on a failed check.
pub async fn holds_door(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
    day: u32,
    rehearsal: bool,
    now: i64,
) -> Result<bool, diesel::result::Error> {
    #[derive(QueryableByName)]
    struct Live {
        #[diesel(sql_type = SqlUuid)]
        user_id: Uuid,
        #[diesel(sql_type = SqlUuid)]
        alt: Uuid,
    }
    #[derive(QueryableByName)]
    struct Holder {
        #[diesel(sql_type = SqlUuid)]
        alt_uuid: Uuid,
    }
    let day = i16::try_from(day).unwrap_or(i16::MAX);
    let Some(live): Option<Live> = diesel::sql_query(
        "SELECT user_id, COALESCE(source_alt_uuid, '00000000-0000-0000-0000-000000000000'::uuid) AS alt \
           FROM characters WHERE id = $1",
    )
    .bind::<SqlUuid, _>(character_id)
    .get_result(conn)
    .await
    .optional()?
    else {
        return Ok(false);
    };
    // Waits for a concurrent binder of the same (account, day) to commit, so
    // the read below sees whichever row won.
    diesel::sql_query(
        "INSERT INTO december_2026_door_holders \
                (user_id, day, rehearsal, alt_uuid, character_id, bound_at) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (user_id, day, rehearsal) DO NOTHING",
    )
    .bind::<SqlUuid, _>(live.user_id)
    .bind::<Int2, _>(day)
    .bind::<Bool, _>(rehearsal)
    .bind::<SqlUuid, _>(live.alt)
    .bind::<SqlUuid, _>(character_id)
    .bind::<Int8, _>(now)
    .execute(conn)
    .await?;
    let holder: Option<Holder> = diesel::sql_query(
        "SELECT alt_uuid FROM december_2026_door_holders \
          WHERE user_id = $1 AND day = $2 AND rehearsal = $3",
    )
    .bind::<SqlUuid, _>(live.user_id)
    .bind::<Int2, _>(day)
    .bind::<Bool, _>(rehearsal)
    .get_result(conn)
    .await
    .optional()?;
    Ok(holder.is_some_and(|h| h.alt_uuid == live.alt))
}

/// The door open on this character's calendar clock, if any, and whether this
/// character holds it for its account (binding it if nobody does yet). A failed
/// check is logged and read as "not the holder".
pub async fn todays_door(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
    calendar_now: i64,
    real_now: i64,
) -> Option<(&'static december_2026::Door, bool)> {
    let door = december_2026::open_doors(calendar_now).next()?;
    let rehearsal = calendar_now != real_now;
    let holds = match holds_door(conn, character_id, door.day, rehearsal, real_now).await {
        Ok(holds) => holds,
        Err(e) => {
            log::warn!(
                "[december] door {} holder check failed for {character_id}; not offering it: {e}",
                door.day
            );
            false
        }
    };
    Some((door, holds))
}

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

/// The banner for today's door, if this character can open it and has not.
///
/// `calendar_now` is the clock the calendar runs on for this character and
/// `real_now` the wall clock; they differ only for a rehearsing test character.
/// `holds_today` is [`holds_door`] for the door open at `calendar_now`: an alt
/// that does not hold it is told about nothing — no banner at all, exactly as
/// once a door is opened. A door whose id is already advertised — an authored
/// `gift_overrides` row replacing it — is not advertised twice.
pub fn door_announcements(
    claims: &HashMap<Uuid, u64>,
    advertised: &HashSet<String>,
    calendar_now: i64,
    real_now: i64,
    holds_today: bool,
) -> Vec<Announcement> {
    let rehearsing = calendar_now != real_now;
    december_2026::open_doors(calendar_now)
        .filter(|door| {
            let id = door.gift.global_gift_id;
            holds_today
                && claims.get(&id).copied().unwrap_or(0) < door.gift.claim_count_limit
                && !advertised.contains(&id.to_string())
        })
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

    /// The banners of a character that holds today's door.
    fn announced(claimed: &[u32], now: i64) -> Vec<Announcement> {
        let claims: HashMap<Uuid, u64> = claimed.iter().map(|d| (door_gift_id(*d), 1)).collect();
        door_announcements(&claims, &HashSet::new(), now, now, true)
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
    /// and lives exactly as long as the door: its own UTC day.
    #[test]
    fn a_banner_arms_its_own_door_and_expires_with_its_day() {
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
        assert_eq!(a.ttl, at(2, 0), "door 1 closes at the end of 1 December");

        let eve = announced(&[], at(31, 9));
        assert_eq!(days(&eve), vec![31]);
        assert_eq!(eve[0].start_time, at(31, 0));
        assert_eq!(eve[0].ttl, at(32, 0), "New Year's Eve gift: the 31st only");
    }

    /// ONLY TODAY'S DOOR, whatever was missed: a player who skipped every day
    /// so far is shown today's door and nothing else, and yesterday's is gone
    /// from 00:00:00 to the second.
    #[test]
    fn only_todays_door_is_advertised_and_a_missed_day_is_gone() {
        for day in 1..=24u32 {
            assert_eq!(days(&announced(&[], at(day, 7))), vec![day]);
            assert!(
                announced(&[day], at(day, 8)).is_empty(),
                "{day} December, opened"
            );
        }
        assert_eq!(days(&announced(&[], at(6, 0) - 1)), vec![5]);
        assert_eq!(days(&announced(&[], at(6, 0))), vec![6]);
    }

    /// 25-30 December bring no door, and nothing is left over from the 24th.
    #[test]
    fn nothing_between_christmas_and_new_years_eve() {
        for day in 25..=30 {
            assert!(announced(&[], at(day, 12)).is_empty(), "{day} December");
        }
    }

    /// From 1 January nothing, claimed or not.
    #[test]
    fn nothing_is_advertised_once_the_calendar_ends() {
        assert!(announced(&[], at(32, 0)).is_empty());
        assert!(announced(&[], at(32, 12)).is_empty());
        assert!(announced(&[], at(33, 0) + 300 * DAY).is_empty());
    }

    /// AN ALT THAT DOES NOT HOLD TODAY'S DOOR SEES NO BANNER AT ALL — the same
    /// empty list it gets once a door is opened, which the client already
    /// handles every day.
    #[test]
    fn an_alt_that_does_not_hold_the_door_is_told_nothing() {
        for day in [1, 6, 24, 31] {
            let banners = door_announcements(
                &HashMap::new(),
                &HashSet::new(),
                at(day, 12),
                at(day, 12),
                false,
            );
            assert!(banners.is_empty(), "{day} December");
        }
    }

    /// A door replaced by an authored gift of the same id is advertised once,
    /// by the authored row.
    #[test]
    fn a_door_already_advertised_is_not_advertised_twice() {
        let taken: HashSet<String> = [door_gift_id(3).to_string()].into();
        let banners = door_announcements(&HashMap::new(), &taken, at(3, 0), at(3, 0), true);
        assert!(banners.is_empty());
    }

    /// A rehearsal shows a December day's door today, dated around today so
    /// the client treats it as live.
    #[test]
    fn a_rehearsed_door_is_dated_around_the_real_clock() {
        let real = DECEMBER_1_SECS - 5 * DAY;
        let banners = door_announcements(&HashMap::new(), &HashSet::new(), at(6, 10), real, true);
        assert_eq!(days(&banners), vec![6]);
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

    /// The one-door-per-account rule against a real Postgres; SKIPS without
    /// TEST_DATABASE_URL (CI provides one).
    mod db {
        use super::*;
        use diesel_async::{AsyncConnection, SimpleAsyncConnection};

        /// `characters` WITHOUT the one-row-per-user constraint, so the race
        /// test can put two alts of one account live at the same instant —
        /// the worst case two phones on one account could produce.
        const SCHEMA: &str = "
            CREATE TABLE characters (
                id UUID PRIMARY KEY,
                user_id UUID NOT NULL,
                source_alt_uuid UUID);
        ";
        const MIGRATION: &str = include_str!(
            "../../migrations/2026-11-24-000000-0000_december_2026_door_holders/up.sql"
        );

        async fn connect() -> Option<AsyncPgConnection> {
            let url = std::env::var("TEST_DATABASE_URL").ok()?;
            Some(
                AsyncPgConnection::establish(&url)
                    .await
                    .expect("TEST_DATABASE_URL is set but unreachable"),
            )
        }

        async fn fixture() -> Option<AsyncPgConnection> {
            let mut conn = connect().await?;
            conn.begin_test_transaction().await.unwrap();
            let schema = format!("t{}", Uuid::new_v4().simple());
            conn.batch_execute(&format!(
                "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema}; {SCHEMA} {MIGRATION}"
            ))
            .await
            .unwrap();
            Some(conn)
        }

        macro_rules! db {
            () => {
                match fixture().await {
                    Some(c) => c,
                    None => {
                        eprintln!(
                            "SKIP: TEST_DATABASE_URL unset — December door holders NOT verified"
                        );
                        return;
                    }
                }
            };
        }

        async fn put(conn: &mut AsyncPgConnection, id: Uuid, user: Uuid, alt: Option<Uuid>) {
            diesel::sql_query(
                "INSERT INTO characters (id, user_id, source_alt_uuid) VALUES ($1, $2, $3) \
                 ON CONFLICT (id) DO UPDATE SET source_alt_uuid = EXCLUDED.source_alt_uuid",
            )
            .bind::<SqlUuid, _>(id)
            .bind::<SqlUuid, _>(user)
            .bind::<diesel::sql_types::Nullable<SqlUuid>, _>(alt)
            .execute(conn)
            .await
            .unwrap();
        }

        fn ids() -> (Uuid, Uuid, Uuid, Uuid) {
            (
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            )
        }

        /// THE FIRST CHARACTER OF THE DAY WINS, the others get nothing: alt A is
        /// loaded first on the 6th and holds door 6; the player switches to alt
        /// B — B does not hold it; back to A — A still does. A holding nothing
        /// for the 7th yet, B loaded first on the 7th takes door 7.
        #[tokio::test]
        async fn the_first_alt_of_the_day_holds_the_door_and_a_switch_changes_nothing() {
            let mut c = db!();
            let (row, user, a, b) = ids();
            let t = DECEMBER_1_SECS + 5 * DAY;
            put(&mut c, row, user, Some(a)).await;
            assert!(
                holds_door(&mut c, row, 6, false, t).await.unwrap(),
                "A, first"
            );
            assert!(
                holds_door(&mut c, row, 6, false, t + 1).await.unwrap(),
                "A, again"
            );

            put(&mut c, row, user, Some(b)).await;
            assert!(
                !holds_door(&mut c, row, 6, false, t + 60).await.unwrap(),
                "B, second"
            );

            put(&mut c, row, user, Some(a)).await;
            assert!(
                holds_door(&mut c, row, 6, false, t + 120).await.unwrap(),
                "back to A"
            );

            put(&mut c, row, user, Some(b)).await;
            assert!(
                holds_door(&mut c, row, 7, false, t + DAY).await.unwrap(),
                "B on the 7th"
            );
            put(&mut c, row, user, Some(a)).await;
            assert!(
                !holds_door(&mut c, row, 7, false, t + DAY + 5)
                    .await
                    .unwrap(),
                "A on the 7th"
            );
        }

        /// CONTROL: the rule is per account, not global. Two accounts each
        /// have their own holder of the same day, and a row from before alts
        /// were tracked (no alt uuid) holds its door like any other.
        #[tokio::test]
        async fn every_account_has_its_own_holder() {
            let mut c = db!();
            let (row1, user1, a, _) = ids();
            let (row2, user2, _, _) = ids();
            put(&mut c, row1, user1, Some(a)).await;
            put(&mut c, row2, user2, None).await;
            assert!(
                holds_door(&mut c, row1, 1, false, DECEMBER_1_SECS)
                    .await
                    .unwrap()
            );
            assert!(
                holds_door(&mut c, row2, 1, false, DECEMBER_1_SECS)
                    .await
                    .unwrap()
            );
            assert!(
                holds_door(&mut c, row2, 1, false, DECEMBER_1_SECS + 9)
                    .await
                    .unwrap()
            );
            assert!(
                !holds_door(&mut c, Uuid::new_v4(), 1, false, DECEMBER_1_SECS)
                    .await
                    .unwrap(),
                "no row"
            );
        }

        /// A rehearsal does not take the real day: a test character that
        /// rehearsed the 6th in November is still free to be first on the
        /// real 6 December — and so is its account's other alt.
        #[tokio::test]
        async fn a_rehearsal_does_not_hold_the_real_days_door() {
            let mut c = db!();
            let (row, user, a, b) = ids();
            put(&mut c, row, user, Some(a)).await;
            assert!(
                holds_door(&mut c, row, 6, true, DECEMBER_1_SECS - 3 * DAY)
                    .await
                    .unwrap()
            );
            put(&mut c, row, user, Some(b)).await;
            assert!(
                holds_door(&mut c, row, 6, false, DECEMBER_1_SECS + 5 * DAY)
                    .await
                    .unwrap()
            );
        }

        /// THE MIDNIGHT RACE: two alts of one account live at the same instant
        /// (two phones), both asking for every door at once on their own
        /// connections. Each door ends up with exactly one holder.
        #[tokio::test]
        async fn two_alts_racing_never_both_hold_a_door() {
            let (Some(mut setup), Some(mut c1), Some(mut c2)) =
                (connect().await, connect().await, connect().await)
            else {
                eprintln!("SKIP: TEST_DATABASE_URL unset — door holder race NOT verified");
                return;
            };
            // Committed, on its own schema, so the two racers see one table;
            // dropped at the end whatever happens in between.
            let schema = format!("t{}", Uuid::new_v4().simple());
            setup
                .batch_execute(&format!(
                    "CREATE SCHEMA {schema}; SET search_path TO {schema}; {SCHEMA} {MIGRATION}"
                ))
                .await
                .unwrap();
            let (row_a, row_b, user, alt_a) = ids();
            let alt_b = Uuid::new_v4();
            put(&mut setup, row_a, user, Some(alt_a)).await;
            put(&mut setup, row_b, user, Some(alt_b)).await;
            for c in [&mut c1, &mut c2] {
                c.batch_execute(&format!("SET search_path TO {schema}"))
                    .await
                    .unwrap();
            }

            let race = async {
                let mut outcomes = Vec::new();
                for day in 1..=24u32 {
                    let t = DECEMBER_1_SECS + i64::from(day - 1) * DAY;
                    let (x, y) = tokio::join!(
                        holds_door(&mut c1, row_a, day, false, t),
                        holds_door(&mut c2, row_b, day, false, t),
                    );
                    outcomes.push((day, x, y));
                }
                outcomes
            }
            .await;
            setup
                .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
                .await
                .unwrap();

            for (day, x, y) in race {
                let (x, y) = (x.unwrap(), y.unwrap());
                assert!(x ^ y, "door {day}: exactly one holder, got A={x} B={y}");
            }
        }
    }
}
