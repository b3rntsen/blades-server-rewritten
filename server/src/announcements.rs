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
    let now = now_secs();
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

    // Last, and for every source at once: put the asset on a host the client can
    // actually resolve. Done here rather than at each `push` so a new kind of
    // announcement cannot be added on an unreachable host by omission.
    for a in &mut announcements {
        a.asset_url = reachable_asset_url(&a.asset_url, asset_base());
    }

    Ok(Json(AnnouncementsResponse { announcements }))
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
    let start = if gift.start_time > 0 { gift.start_time } else { now };
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
        let a = season_reward_announcement(&season);
        assert_eq!(a.id, id.to_string());
        assert!(a.asset_url.ends_with(&format!("/arena-season/{id}")));
        assert_eq!(a.start_time, 210);
        assert_eq!(a.ttl, 210 + REWARD_ANNOUNCEMENT_LIFETIME);
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
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../deploy/static/announcements.json");
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
        assert_eq!(rewritten, 156, "every shipped url was minted on the dead host");
    }

    /// The default must be the host that actually answers, because an operator
    /// who sets nothing is the normal case — and the previous default was a
    /// name that does not resolve.
    #[test]
    fn the_default_origin_is_the_edge_that_serves_these() {
        assert_eq!(DEFAULT_ASSET_ORIGIN, "https://announcements-feed.nb.dethele.com");
        assert_ne!(DEFAULT_ASSET_ORIGIN, RETAIL_ASSET_ORIGIN);
    }
}
