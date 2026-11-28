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
    let owner = get_only_single_character_and_check_permission(owners, &session.session)?;

    let pending = pending_reward_seasons(&mut conn, character_id).await?;

    let now = now_secs();
    let mut announcements = app_state.static_data.announcements.clone();
    announcements.extend(
        pending
            .iter()
            .map(|season| season_reward_announcement(season, now)),
    );

    // A Free for All the client cannot see is a giveaway nobody collects: gift
    // ids are only discoverable through this feed. Derived from the open run
    // rather than stored alongside it, so the advert and the gift cannot drift.
    if let Some(run) = crate::free_for_all::open_run(&mut conn, now).await {
        announcements.push(free_for_all_announcement(&run));
    }

    // Hand-authored gifts, same reasoning as the line above and the reason it
    // was written: a gift the client cannot see is one nobody collects. The
    // Publish button on /admin/gifts writes a row the server will honour and,
    // until this, nothing told a client the id existed — so it could never be
    // asked for. Derived from the open rows rather than a second table, so the
    // advert and the gift cannot drift apart.
    for gift in crate::free_for_all::open_gifts(&mut conn, now).await {
        announcements.push(authored_gift_announcement(&gift, now));
    }

    // The arena trophy track's town gifts (HauDrauf #625). ONE at a time, as the
    // weapon gift: the oldest unclaimed for the alt playing now; the next
    // appears once it is claimed.
    match crate::arena::trophy_track::pending_for_character(&mut conn, character_id).await {
        Ok(pending) => {
            if let Some((id, gift)) = pending.into_iter().next() {
                announcements.push(trophy_gift_announcement(id, &gift, now));
            }
        }
        Err(e) => log::warn!("announcements: arena trophy gifts unavailable: {e}"),
    }

    // The arena weapon gift (owner-approved #390/#391), for the same reason: its
    // id is only discoverable here. ONE at a time — the oldest unclaimed for the
    // alt playing now; the next appears once it is claimed. A failure (the table
    // not yet migrated) costs only this banner, never the feed.
    match crate::arena::arena_fight_gift::pending_for_live_alt(&mut conn, character_id).await {
        Ok(pending) => {
            if let Some(gift) = pending.into_iter().next() {
                announcements.push(arena_weapon_gift_announcement(&gift, now));
            }
        }
        Err(e) => log::warn!("announcements: arena weapon gifts unavailable: {e}"),
    }

    // The December 2026 gift calendar (tracker #499): today's door, if this
    // character can open it and has not. Compiled in, so before 1 December
    // 00:00 UTC this adds nothing and touches no table. `calendar_now` is the
    // real clock for everyone but a rehearsing test character. This request is
    // where the account's first character of the day is bound to the door
    // (`crate::december`); the account's other alts are shown no banner.
    let calendar_now = crate::december::clock_for(&mut conn, character_id, now).await;
    if let Some((_, holds_today)) =
        crate::december::todays_door(&mut conn, character_id, calendar_now, now).await
    {
        let advertised: std::collections::HashSet<String> =
            announcements.iter().map(|a| a.id.clone()).collect();
        announcements.extend(crate::december::door_announcements(
            &owner.server_state.0.gift_claims,
            &advertised,
            calendar_now,
            now,
            holds_today,
        ));
    }

    // Gifted gear that did not fit when its gift was claimed (tracker #499): its
    // payout is discoverable only here, like every other gift. Advertised only
    // while a claim would hand something over — a piece is due and there is
    // room — so the banner never arms a Claim that answers "not active". The
    // inventory is read only for a character that is owed something.
    if !owner.server_state.0.deferred_gift_gear.is_empty() {
        if let Some((_, owed)) =
            crate::global_gift::deferred_payout(&mut conn, character_id, calendar_now).await
        {
            announcements.push(deferred_gear_announcement(owed, now));
        }
    }

    // Last, and for every source at once: put the asset on a host the client can
    // actually resolve. Done here rather than at each `push` so a new kind of
    // announcement cannot be added on an unreachable host by omission.
    for a in &mut announcements {
        a.asset_url = reachable_asset_url(&a.asset_url, asset_base());
    }

    Ok(Json(AnnouncementsResponse { announcements }))
}

/// The banner for one arena trophy-track town gift (HauDrauf #625). Gift-bearing
/// through the edge's `gift/` namespace like the weapon gift; never closes, so
/// the banner rolls a month ahead.
fn trophy_gift_announcement(
    gift_id: Uuid,
    gift: &blades_lib::server_state::ArenaTrophyGift,
    now: i64,
) -> Announcement {
    Announcement {
        id: gift_id.to_string(),
        r#type: "BASIC".into(),
        start_time: gift.created_at.min(now),
        ttl: now + REWARD_ANNOUNCEMENT_LIFETIME,
        asset_url: format!("https://announcements.blades.bgs.services/gift/{gift_id}"),
    }
}

/// The host every announcement's `assetUrl` was minted on, retail's own.
const RETAIL_ASSET_ORIGIN: &str = "https://announcements.blades.bgs.services";

/// Where those assets actually live for us.
const DEFAULT_ASSET_ORIGIN: &str = "https://announcements-feed.nb.dethele.com";

/// Put an announcement's asset on a host the client can reach.
///
/// THE BUG. An announcement is only ever a pointer: the client takes `assetUrl`,
/// fetches `<assetUrl>/manifest.ms`, and shows nothing at all unless that
/// manifest arrives. Every url we serve — the 156 mined from retail, and the
/// ones we mint for seasons, giveaways and authored gifts — named
/// `announcements.blades.bgs.services`. That host is Bethesda's, it is now a
/// DANGLING CNAME, and from the public internet it does not resolve at all.
///
/// A VPN player never noticed: our own DNS answers for it and the mitm addon
/// serves the same routes. A no-VPN player could not see a single announcement,
/// ever — and the failure is silent, because a name that will not resolve
/// produces no request, no error page and no log line anywhere we look. It took
/// an authored gift that was live, open and correctly advertised to surface it:
/// 30 hours of edge logs hold ZERO announcement-asset fetches from any game
/// client, while the feed itself was served 200 every few minutes.
///
/// The repointed APK rewrites this host inside `global-metadata.dat`, which is
/// why it is in `patch-il2cpp-hosts.py` — but that only rewrites COMPILE-TIME
/// literals. `assetUrl` arrives at runtime, in our own JSON, so the patch never
/// touches it and the server has to hand over a reachable host itself.
///
/// Host-only, and only for that one origin: the path carries the uuid that
/// becomes `GlobalGiftId` and arms the Claim button, so it must survive
/// byte-for-byte, and a url on any other host is left alone rather than
/// rewritten on a guess.
fn reachable_asset_url(url: &str, base: &str) -> String {
    match url.strip_prefix(RETAIL_ASSET_ORIGIN) {
        Some(path) => format!("{}{}", base.trim_end_matches('/'), path),
        None => url.to_string(),
    }
}

/// `ANNOUNCEMENTS_ASSET_ORIGIN`, or the edge host that serves these today.
fn asset_base() -> &'static str {
    static BASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BASE.get_or_init(|| {
        std::env::var("ANNOUNCEMENTS_ASSET_ORIGIN")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_ASSET_ORIGIN.to_string())
    })
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

/// The banner for one hand-authored gift.
///
/// GIFT-BEARING: the edge turns the uuid in this path into the manifest's
/// `GlobalGiftId`, which is what arms the Claim button. That is why the id in
/// the URL must be the gift's own id and not a fresh one.
///
/// `ttl` is the gift's close time, never a fixed lifetime, so the banner cannot
/// outlive the gift — a Claim on a closed gift returns "not active", which
/// reads to a player as the game being broken. A gift with no end date (0, what
/// the admin form writes for "always") gets a rolling month so the feed does
/// not carry a banner forever.
fn authored_gift_announcement(
    gift: &crate::free_for_all::GiftOverrideRow,
    now: i64,
) -> Announcement {
    let start = if gift.start_time > 0 {
        gift.start_time
    } else {
        now
    };
    let ttl = if gift.end_time > 0 {
        gift.end_time
    } else {
        now + REWARD_ANNOUNCEMENT_LIFETIME
    };
    Announcement {
        id: gift.gift_id.to_string(),
        r#type: "BASIC".into(),
        start_time: start,
        ttl,
        asset_url: format!(
            "https://announcements.blades.bgs.services/gift/{}",
            gift.gift_id
        ),
    }
}

/// The banner for one arena weapon gift (owner-approved #390/#391).
///
/// GIFT-BEARING through the edge's existing `gift/` namespace: the uuid in the
/// path becomes the manifest's `GlobalGiftId` and arms the Claim button. The
/// gift never closes, so the banner rolls a month ahead like a dateless
/// authored gift.
fn arena_weapon_gift_announcement(
    gift: &crate::arena::arena_fight_gift::FightGiftRow,
    now: i64,
) -> Announcement {
    Announcement {
        id: gift.id.to_string(),
        r#type: "BASIC".into(),
        start_time: gift.created_at.min(now),
        ttl: now + REWARD_ANNOUNCEMENT_LIFETIME,
        asset_url: format!("https://announcements.blades.bgs.services/gift/{}", gift.id),
    }
}

/// The banner for the deferred-gear payout (tracker #499).
///
/// GIFT-BEARING through the `gift/` namespace, pointing at the payout's fixed
/// gift id. The banner's OWN id carries the number of pieces still owed, so a
/// second payout — after a first one that left pieces on file — is a new
/// announcement to the client rather than one it has already shown.
fn deferred_gear_announcement(owed: u64, now: i64) -> Announcement {
    let gift_id = blades_lib::features::gift_capacity::DEFERRED_GEAR_GIFT_ID;
    let id = uuid::Uuid::from_u128(
        (gift_id.as_u128() & !0xFFFF_FFFF) | u128::from(owed.min(0xFFFF_FFFF)),
    );
    Announcement {
        id: id.to_string(),
        r#type: "BASIC".into(),
        start_time: now,
        ttl: now + REWARD_ANNOUNCEMENT_LIFETIME,
        asset_url: format!("https://announcements.blades.bgs.services/gift/{gift_id}"),
    }
}

/// Ended seasons that still owe the alt playing now an unclaimed award.
///
/// The row filter (`granted_at IS NULL`) is what ends a season banner: once
/// every award is claimed the season drops out here, so the banner's `ttl` no
/// longer has to.
///
/// One indexed existence probe per ended season. The award unique index starts
/// `(season_id, character_id)`, so this remains cheap without adding a
/// production index or running a migration for the feature.
///
/// Only the alt playing now is told about a reward: a user's alts share this
/// character id, and advertising another alt's placing would offer a gift the
/// claim then refuses (owner decision 2026-10-10).
async fn pending_reward_seasons(
    conn: &mut diesel_async::AsyncPgConnection,
    character_id: Uuid,
) -> Result<Vec<season_store::SeasonRow>, diesel::result::Error> {
    diesel::sql_query(format!(
        "SELECT s.* FROM arena_seasons s \
         WHERE s.status = 'ended' AND s.ended_at IS NOT NULL \
           AND EXISTS ( \
             SELECT 1 FROM arena_season_awards a \
             WHERE a.season_id = s.id AND a.character_id = $1 \
               AND a.granted_at IS NULL \
               AND {} \
           ) \
         ORDER BY s.ended_at DESC",
        season_store::AWARD_BELONGS_TO_LIVE_ALT_SQL
    ))
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .load(conn)
    .await
}

/// The banner for one ended season's unclaimed award.
///
/// `ttl` rolls a month ahead of `now`, like the other gift banners. It used to
/// be `ended_at + 31 days`, which hid the banner — the ONLY way to claim a
/// season award — from every player who had not claimed within a month: their
/// awards stayed owed and unclaimable forever. The banner now lasts exactly as
/// long as the award is unclaimed (see `pending_reward_seasons`).
fn season_reward_announcement(season: &season_store::SeasonRow, now: i64) -> Announcement {
    let start = season.ended_at.unwrap_or(season.ends_at);
    Announcement {
        id: season.id.to_string(),
        r#type: "BASIC".into(),
        start_time: start.min(now),
        ttl: now + REWARD_ANNOUNCEMENT_LIFETIME,
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

    fn a_gift(start: i64, end: i64) -> crate::free_for_all::GiftOverrideRow {
        crate::free_for_all::GiftOverrideRow {
            gift_id: uuid::uuid!("6f7c99c5-e403-4928-bc68-a82f6be18ded"),
            items: crate::json_db::JsonDbWrapper(Vec::new()),
            chests: crate::json_db::JsonDbWrapper(Vec::new()),
            start_time: start,
            end_time: end,
            claim_count_limit: 1,
            description: None,
            updated_at: 0,
            updated_by: None,
        }
    }

    /// The banner must carry the GIFT's own id in its path.
    ///
    /// The edge turns that uuid into the manifest's `GlobalGiftId`, which is the
    /// only thing that arms Claim. A fresh id here would render a banner that
    /// looks right and grants nothing — which is exactly the failure this whole
    /// feature exists to fix, so it is asserted rather than assumed.
    #[test]
    fn the_banner_points_at_the_gift_itself() {
        let g = a_gift(100, 200);
        let a = authored_gift_announcement(&g, 150);
        assert_eq!(a.id, g.gift_id.to_string());
        assert!(
            a.asset_url.ends_with(&g.gift_id.to_string()),
            "asset_url {} must end with the gift id",
            a.asset_url
        );
        assert!(a.asset_url.contains("/gift/"), "gift-bearing namespace");
    }

    /// The banner must not outlive the gift. A Claim on a closed gift answers
    /// "not active", which reads to a player as the game being broken.
    #[test]
    fn the_banner_expires_with_the_gift() {
        let a = authored_gift_announcement(&a_gift(100, 200), 150);
        assert_eq!(a.start_time, 100);
        assert_eq!(a.ttl, 200, "ttl is the gift's close time, not a lifetime");
    }

    /// A gift with no dates is "always" in the admin form, which stores 0/0.
    /// Zero must not be read as "expired in 1970" nor advertised forever.
    #[test]
    fn a_dateless_gift_starts_now_and_rolls() {
        let now = 1_789_792_376;
        let a = authored_gift_announcement(&a_gift(0, 0), now);
        assert_eq!(a.start_time, now, "0 start means now, not 1970");
        assert_eq!(a.ttl, now + REWARD_ANNOUNCEMENT_LIFETIME);
        assert!(a.ttl > now, "a dateless gift must still be live");
    }

    /// The arena weapon gift's banner points at the gift itself in the
    /// gift-bearing namespace, and is live now.
    #[test]
    fn the_arena_weapon_gift_banner_arms_its_own_claim() {
        let id = uuid::uuid!("2b6a8e0e-1c51-4b8e-9f4e-3f1d6c0c2a77");
        let gift = crate::arena::arena_fight_gift::FightGiftRow {
            id,
            user_id: uuid::Uuid::nil(),
            character_id: uuid::Uuid::nil(),
            source_alt_uuid: None,
            milestone: 25,
            item: serde_json::json!({}),
            created_at: 1_000,
            granted_at: None,
        };
        let a = arena_weapon_gift_announcement(&gift, 2_000);
        assert_eq!(a.id, id.to_string());
        assert!(
            a.asset_url.ends_with(&format!("/gift/{id}")),
            "{}",
            a.asset_url
        );
        assert!(a.start_time <= 2_000 && a.ttl > 2_000, "live now");
        assert_eq!(
            reachable_asset_url(&a.asset_url, DEFAULT_ASSET_ORIGIN),
            format!("{DEFAULT_ASSET_ORIGIN}/gift/{id}"),
            "moved to the reachable host with the gift id intact"
        );
    }

    /// The deferred-gear banner arms the payout gift's Claim, and a later payout
    /// is a different announcement from the one before it.
    #[test]
    fn the_deferred_gear_banner_points_at_the_payout_gift() {
        let gift_id = blades_lib::features::gift_capacity::DEFERRED_GEAR_GIFT_ID;
        let first = deferred_gear_announcement(12, 5_000);
        let second = deferred_gear_announcement(4, 6_000);
        for a in [&first, &second] {
            assert!(
                a.asset_url.ends_with(&format!("/gift/{gift_id}")),
                "{}",
                a.asset_url
            );
            assert!(uuid::Uuid::parse_str(&a.id).is_ok(), "{} is a uuid", a.id);
            assert!(a.start_time <= 6_000 && a.ttl > 6_000, "live now");
        }
        assert_ne!(first.id, second.id);
        assert_eq!(first.id, deferred_gear_announcement(12, 9_000).id);
    }

    /// CONTROL: the Free for All banner is deliberately NOT gift-bearing, and
    /// must stay on its own namespace. If these two ever shared one, the
    /// giveaway would grow a Claim button that posts to a gift that does not
    /// exist.
    #[test]
    fn the_free_for_all_banner_is_a_different_namespace() {
        let gift = authored_gift_announcement(&a_gift(100, 200), 150);
        assert!(!gift.asset_url.contains("/free-for-all/"));
        assert!(!gift.asset_url.contains("/arena-season/"));
    }

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
        let a = season_reward_announcement(&season, 300);
        assert_eq!(a.id, id.to_string());
        assert!(a.asset_url.ends_with(&format!("/arena-season/{id}")));
        assert_eq!(a.start_time, 210);
        assert_eq!(a.ttl, 300 + REWARD_ANNOUNCEMENT_LIFETIME);
    }

    /// THE BUG (d130): Season 1 ended 2026-10-04; on 2026-11-28 a player with
    /// two unclaimed awards got a banner whose ttl (ended_at + 31 days) had
    /// passed, so the client never showed it and the awards could not be
    /// claimed at all. An unclaimed award keeps its banner live.
    #[test]
    fn a_season_ended_long_ago_still_shows_its_unclaimed_banner() {
        let ended = 1_791_130_204; // 2026-10-04 16:10:04 UTC
        let now = 1_795_858_620; // 2026-11-28, 54 days later
        let season = season_store::SeasonRow {
            id: Uuid::from_u128(1),
            number: 1,
            name: "Season 1".into(),
            starts_at: ended - 1000,
            ends_at: ended,
            status: "ended".into(),
            scoring: "shipped".into(),
            reset_rule: "hard_reset".into(),
            created_at: ended - 2000,
            ended_at: Some(ended),
        };
        let a = season_reward_announcement(&season, now);
        assert!(
            a.start_time <= now && now < a.ttl,
            "banner must be live now"
        );
    }
}

#[cfg(test)]
mod pending_season_db_tests {
    use super::*;
    use diesel::sql_types::{BigInt, Nullable, Uuid as SqlUuid};
    use diesel_async::{AsyncConnection, AsyncPgConnection, SimpleAsyncConnection};

    const SCHEMA: &str = "
        CREATE TABLE characters (id UUID PRIMARY KEY, source_alt_uuid UUID);
        CREATE TABLE arena_seasons (
            id UUID PRIMARY KEY, number INTEGER NOT NULL, name TEXT NOT NULL,
            starts_at BIGINT NOT NULL, ends_at BIGINT NOT NULL,
            status TEXT NOT NULL DEFAULT 'scheduled', scoring TEXT NOT NULL DEFAULT 'shipped',
            reset_rule TEXT NOT NULL DEFAULT 'hard_reset', created_at BIGINT NOT NULL DEFAULT 0,
            ended_at BIGINT);
        CREATE TABLE arena_season_awards (
            id UUID PRIMARY KEY, season_id UUID NOT NULL, character_id UUID NOT NULL,
            source_alt_uuid UUID, granted_at BIGINT);
    ";

    async fn fixture() -> Option<AsyncPgConnection> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let mut conn = AsyncPgConnection::establish(&url)
            .await
            .expect("TEST_DATABASE_URL is set but unreachable");
        conn.begin_test_transaction().await.unwrap();
        let schema = format!("t{}", Uuid::new_v4().simple());
        conn.batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema};"
        ))
        .await
        .unwrap();
        conn.batch_execute(SCHEMA).await.unwrap();
        Some(conn)
    }

    async fn seed(
        conn: &mut AsyncPgConnection,
        character: Uuid,
        season: Uuid,
        granted: Option<i64>,
    ) {
        diesel::sql_query("INSERT INTO characters (id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind::<SqlUuid, _>(character)
            .execute(conn)
            .await
            .unwrap();
        // Ended 2026-10-04: more than 31 days before the bug was seen.
        diesel::sql_query(
            "INSERT INTO arena_seasons (id, number, name, starts_at, ends_at, status, ended_at) \
             VALUES ($1, 1, 'Season 1', 1789000000, 1791130204, 'ended', 1791130204) \
             ON CONFLICT DO NOTHING",
        )
        .bind::<SqlUuid, _>(season)
        .execute(conn)
        .await
        .unwrap();
        diesel::sql_query(
            "INSERT INTO arena_season_awards (id, season_id, character_id, granted_at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind::<SqlUuid, _>(Uuid::new_v4())
        .bind::<SqlUuid, _>(season)
        .bind::<SqlUuid, _>(character)
        .bind::<Nullable<BigInt>, _>(granted)
        .execute(conn)
        .await
        .unwrap();
    }

    /// The failing case end to end: a season that ended > 31 days ago with an
    /// unclaimed legacy award is still served, with a banner that is live now;
    /// once claimed it is not served at all.
    #[actix_web::test]
    async fn an_old_unclaimed_award_is_served_and_a_claimed_one_is_not() {
        let Some(mut conn) = fixture().await else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — season banner NOT verified");
            return;
        };
        let (owed, done, season) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        seed(&mut conn, owed, season, None).await;
        seed(&mut conn, done, season, Some(1_792_000_000)).await;
        let now = 1_795_858_620; // 2026-11-28

        let pending = pending_reward_seasons(&mut conn, owed).await.unwrap();
        assert_eq!(pending.len(), 1);
        let a = season_reward_announcement(&pending[0], now);
        assert!(
            a.start_time <= now && now < a.ttl,
            "unclaimed banner must be live"
        );

        assert!(
            pending_reward_seasons(&mut conn, done)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(test)]
mod asset_host_tests {
    use super::*;

    const OURS: &str = "https://announcements-feed.nb.dethele.com";

    /// THE BUG: every url we serve pointed at a host that does not resolve, so
    /// no no-VPN client could fetch a manifest, so no announcement was ever
    /// shown — including a live, open, correctly-advertised gift.
    #[test]
    fn a_retail_url_moves_to_a_host_that_resolves() {
        assert_eq!(
            reachable_asset_url(
                "https://announcements.blades.bgs.services/gift/6f7c99c5-e403-4928-bc68-a82f6be18ded",
                OURS,
            ),
            "https://announcements-feed.nb.dethele.com/gift/6f7c99c5-e403-4928-bc68-a82f6be18ded",
        );
    }

    /// THE PROPERTY THAT MATTERS MOST. The uuid in the path becomes the
    /// manifest's `GlobalGiftId`, which is the only thing that arms Claim. A
    /// rewrite that touched the path would leave a banner that grants nothing.
    #[test]
    fn the_path_survives_byte_for_byte() {
        for path in [
            "/gift/6f7c99c5-e403-4928-bc68-a82f6be18ded",
            "/arena-season/11111111-2222-4333-8444-555555555555",
            "/free-for-all/99999999-8888-4777-8666-555555555555",
            "/2026/02/26/a99973b7-459f-4aed-9bce-c32f5d57ab30",
        ] {
            let out = reachable_asset_url(&format!("{RETAIL_ASSET_ORIGIN}{path}"), OURS);
            assert_eq!(out, format!("{OURS}{path}"), "path changed for {path}");
        }
    }

    /// CONTROL: only that one origin is touched. A url already on our host, or
    /// on anything else, is left exactly as it is — this must not become a
    /// blanket rewriter that redirects whatever it is handed.
    #[test]
    fn every_other_host_is_left_alone() {
        for url in [
            "https://announcements-feed.nb.dethele.com/gift/6f7c99c5-e403-4928-bc68-a82f6be18ded",
            "https://example.invalid/gift/6f7c99c5-e403-4928-bc68-a82f6be18ded",
            "http://announcements.blades.bgs.services/gift/x", // http, not https
            "",
        ] {
            assert_eq!(reachable_asset_url(url, OURS), url);
        }
    }

    /// A trailing slash on the configured origin must not produce `//gift/…`:
    /// the edge's location regexes are anchored at `^/gift/`, so a double slash
    /// falls through to the inert template and the banner silently dies again.
    #[test]
    fn a_trailing_slash_does_not_double_up() {
        assert_eq!(
            reachable_asset_url(
                "https://announcements.blades.bgs.services/gift/6f7c99c5-e403-4928-bc68-a82f6be18ded",
                "https://announcements-feed.nb.dethele.com/",
            ),
            "https://announcements-feed.nb.dethele.com/gift/6f7c99c5-e403-4928-bc68-a82f6be18ded",
        );
    }

    /// And the shipped corpus, because the mined feed is the bulk of it: all 156
    /// retail announcements were minted on the dead host, and every one of them
    /// has to come out reachable. If this number moves, the file was regenerated
    /// — check the new entries carry a host a client can resolve.
    #[test]
    fn the_whole_shipped_feed_becomes_reachable() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/static/announcements.json"
        );
        let raw = std::fs::read_to_string(path).expect("read announcements.json");
        let feed: Vec<Announcement> = serde_json::from_str(&raw).expect("parse announcements");
        assert_eq!(feed.len(), 156, "the shipped retail feed");

        let mut rewritten = 0;
        for a in &feed {
            let out = reachable_asset_url(&a.asset_url, OURS);
            assert!(
                out.starts_with(OURS),
                "{} still points at a host no client can resolve",
                a.id
            );
            if out != a.asset_url {
                rewritten += 1;
            }
        }
        assert_eq!(
            rewritten, 156,
            "every shipped url was minted on the dead host"
        );
    }

    /// The default must be the host that actually answers, because an operator
    /// who sets nothing is the normal case — and the previous default was a
    /// name that does not resolve.
    #[test]
    fn the_default_origin_is_the_edge_that_serves_these() {
        assert_eq!(
            DEFAULT_ASSET_ORIGIN,
            "https://announcements-feed.nb.dethele.com"
        );
        assert_ne!(DEFAULT_ASSET_ORIGIN, RETAIL_ASSET_ORIGIN);
    }
}
