//! Arena Giveaway — the recurring "turn up and earn Gems" window.
//!
//! Retail's banner, read off player footage (`youtu.be/GiJDWvCEeXE`, 78-84s):
//!
//! > **Arena Giveaway** — "Mark your calendar - some of the mightiest competitors
//! > are gathering in the Arena to battle this Saturday from 10am-12pm ET. Join
//! > them during that time and earn free Gems!"  `[OK]`
//!
//! The button is **OK**. The banner advertises; it grants nothing. Gems are
//! EARNED by playing inside the two-hour window, which is why this hooks
//! [`crate::arena::arena_economy`] and not the global-gift channel — an earlier
//! cut of this shipped it as a claimable gift and was simply wrong about the
//! mechanic.
//!
//! Three tables:
//!
//! * **`free_for_all_runs`** — one row per window opened. The doubling after a
//!   skipped occurrence is derived from the gaps between these rows, so "we
//!   forgot last month" is a fact in the data rather than a flag someone sets.
//! * **`free_for_all_grants`** — who has already earned this window. Its primary
//!   key is the idempotency guard: five matches inside the window pay once.
//! * **`gift_overrides`** — unrelated to the giveaway, shipped here because the
//!   same investigation exposed the gap: gifts live only in a bind-mounted
//!   `gifts.json` read at process start, so handing anything out by hand meant
//!   editing a file on the box and restarting the server. A row here wins over
//!   that file, and a row for an unknown id simply IS a new gift.
//!
//! The scheduling arithmetic is [`blades_lib::features::free_for_all`]; this
//! module is IO only.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_web::{
    HttpRequest, get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::economy::GEMS;
use blades_lib::features::free_for_all::{self as ffa, Cadence, Window};
use blades_lib::static_data::{GiftChest, GiftDef, GiftItem};
use diesel::{BoolExpressionMethods, ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{BladeApiError, ServerGlobal, admin::check_import_token, json_db::JsonDbWrapper};

/// Out-of-band service id for this module's error envelopes.
const FFA_SERVICE_ID: u64 = 9004;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn bad(code: u64, status: StatusCode) -> BladeApiError {
    BladeApiError::new(status, FFA_SERVICE_ID, code)
}

fn cadence_from_str(s: &str) -> Cadence {
    match s {
        "every_saturday" => Cadence::EverySaturday,
        _ => Cadence::FirstSaturdayOfMonth,
    }
}

fn cadence_to_str(c: Cadence) -> &'static str {
    match c {
        Cadence::EverySaturday => "every_saturday",
        Cadence::FirstSaturdayOfMonth => "first_saturday",
    }
}

// --- gifts in the database ---------------------------------------------------

/// One `gift_overrides` row, as diesel sees it.
#[derive(Debug, diesel::Queryable, diesel::Selectable, diesel::Insertable, diesel::AsChangeset)]
#[diesel(table_name = crate::schema::gift_overrides)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct GiftOverrideRow {
    pub gift_id: Uuid,
    pub items: JsonDbWrapper<Vec<GiftItem>>,
    pub chests: JsonDbWrapper<Vec<GiftChest>>,
    pub start_time: i64,
    pub end_time: i64,
    pub claim_count_limit: i64,
    pub description: Option<String>,
    pub updated_at: i64,
    pub updated_by: Option<String>,
}

impl GiftOverrideRow {
    fn to_def(&self) -> GiftDef {
        GiftDef {
            global_gift_id: self.gift_id,
            items: self.items.0.clone(),
            chests: self.chests.0.clone(),
            start_time: self.start_time,
            end_time: self.end_time,
            // BIGINT in the database, u64 in GiftDef. A hand-edited negative row
            // must clamp to zero, not wrap to 18 quintillion free claims.
            claim_count_limit: self.claim_count_limit.max(0) as u64,
            description: self.description.clone(),
        }
    }
}

/// The gift definition in force for `gift_id`: the database row if there is one,
/// otherwise the static catalogue.
///
/// Every `globalgifts` handler goes through here. A handler that read
/// `static_data.gifts` directly would serve the payload this process booted with
/// and would miss every gift created at runtime.
pub async fn effective_gift(
    app_state: &ServerGlobal,
    conn: &mut AsyncPgConnection,
    gift_id: Uuid,
) -> Option<GiftDef> {
    use crate::schema::gift_overrides::dsl as go;
    let row: Option<GiftOverrideRow> = go::gift_overrides
        .filter(go::gift_id.eq(gift_id))
        .select(GiftOverrideRow::as_select())
        .first(conn)
        .await
        .ok();
    match row {
        Some(r) => Some(r.to_def()),
        None => app_state.static_data.gifts.get(&gift_id).cloned(),
    }
}

/// Every authored gift whose window covers `now`.
///
/// A gift is only discoverable through the announcements feed, so this is what
/// the feed advertises. Reading it from `gift_overrides` rather than from a
/// second "what to advertise" table is deliberate: the banner and the gift then
/// cannot drift, and a gift that closes stops being advertised by the same
/// clause that stops it being claimable.
///
/// `start_time`/`end_time` of 0 mean "always", which is what the admin form
/// writes for a gift with both dates blank — so those are open, not expired.
pub async fn open_gifts(conn: &mut AsyncPgConnection, now: i64) -> Vec<GiftOverrideRow> {
    use crate::schema::gift_overrides::dsl as go;
    go::gift_overrides
        .filter(go::start_time.le(now).or(go::start_time.eq(0)))
        .filter(go::end_time.gt(now).or(go::end_time.eq(0)))
        .select(GiftOverrideRow::as_select())
        .load(conn)
        .await
        .unwrap_or_default()
}

// --- publishing a gift without a restart -------------------------------------

/// One gift in a publish request. The field names are the wire shape the admin
/// UI already stores, so the body is the authored payload verbatim.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftPublishItem {
    pub global_gift_id: Uuid,
    #[serde(default)]
    pub items: Vec<GiftItem>,
    #[serde(default)]
    pub chests: Vec<GiftChest>,
    #[serde(default)]
    pub start_time: i64,
    #[serde(default)]
    pub end_time: i64,
    #[serde(default = "one")]
    pub claim_count_limit: i64,
    #[serde(default)]
    pub description: Option<String>,
}

fn one() -> i64 {
    1
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftPublishRequest {
    pub gifts: Vec<GiftPublishItem>,
    /// Dry run unless set, matching the giveaway console.
    #[serde(default)]
    pub apply: bool,
    #[serde(default)]
    pub published_by: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GiftPublishResponse {
    pub applied: bool,
    pub written: usize,
    pub gift_ids: Vec<Uuid>,
    pub summary: String,
}

/// The first gift in a publish that would hand over nothing, if any.
///
/// An empty gift is a claim button that gives air. `build-gifts-static.py`
/// already refuses to ship one — it says so at length in its own header — and a
/// publish path that did not would simply be the easier way to make the same
/// mistake.
fn first_empty_gift(gifts: &[GiftPublishItem]) -> Option<Uuid> {
    gifts
        .iter()
        .find(|g| g.items.is_empty() && g.chests.is_empty())
        .map(|g| g.global_gift_id)
}

/// The stored claim limit for an authored one.
///
/// Clamped on the way IN as well as on the way out. `GiftOverrideRow::to_def`
/// already clamps when reading, but a negative in the table read back as u64 is
/// 18 quintillion free claims, and one typo should not be one missing clamp away
/// from that.
fn stored_claim_limit(authored: i64) -> i64 {
    authored.max(0)
}

/// `POST /…/api/dev/v1/gifts` — publish authored gifts straight into the
/// database, where `effective_gift` already looks first.
///
/// WHY THIS EXISTS
///
/// A gift authored in the admin UI used to reach players only after somebody ran
/// `scripts/build-gifts-static.py` and redeployed the arena, because gifts lived
/// in a bind-mounted `gifts.json` read once at process start. The admin page
/// said so in a banner. That is a deploy to hand somebody a hat.
///
/// The database half already existed — `gift_overrides` and `effective_gift`,
/// added so the giveaway could hand things out at runtime. This is the missing
/// write path: the UI posts the payload it already has, the row lands, and the
/// very next request serves it. No file, no regeneration, no restart.
///
/// The static catalogue stays as the floor: `effective_gift` falls back to it,
/// so a gift that has never been published behaves exactly as before.
#[post("/blades.bgs.services/api/dev/v1/gifts")]
pub async fn publish_gifts(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: Json<GiftPublishRequest>,
) -> Result<Json<GiftPublishResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let body = body.into_inner();

    if body.gifts.is_empty() {
        return Err(bad(6, StatusCode::BAD_REQUEST));
    }

    let now = now_secs();
    let ids: Vec<Uuid> = body.gifts.iter().map(|g| g.global_gift_id).collect();

    if let Some(empty) = first_empty_gift(&body.gifts) {
        log::warn!(
            "[gifts] refusing to publish {empty} with no items and no chests"
        );
        return Err(bad(7, StatusCode::BAD_REQUEST));
    }

    let summary = format!(
        "{} gift(s): {}",
        body.gifts.len(),
        body.gifts
            .iter()
            .map(|g| {
                format!(
                    "{} ({} item(s), {} chest(s))",
                    g.global_gift_id,
                    g.items.len(),
                    g.chests.len()
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    );

    if !body.apply {
        return Ok(Json(GiftPublishResponse {
            applied: false,
            written: 0,
            gift_ids: ids,
            summary,
        }));
    }

    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| bad(5, StatusCode::SERVICE_UNAVAILABLE))?;

    use crate::schema::gift_overrides::dsl as go;
    let mut written = 0usize;
    for g in &body.gifts {
        let row = GiftOverrideRow {
            gift_id: g.global_gift_id,
            items: JsonDbWrapper(g.items.clone()),
            chests: JsonDbWrapper(g.chests.clone()),
            start_time: g.start_time,
            end_time: g.end_time,
            // Clamp on the way in as well as on the way out: a negative limit
            // read back as u64 is 18 quintillion free claims, and the read-side
            // clamp in `to_def` should not be the only thing standing between a
            // typo and that.
            claim_count_limit: stored_claim_limit(g.claim_count_limit),
            description: g.description.clone(),
            updated_at: now,
            updated_by: body.published_by.clone(),
        };
        diesel::insert_into(go::gift_overrides)
            .values(&row)
            .on_conflict(go::gift_id)
            .do_update()
            .set(&row)
            .execute(&mut conn)
            .await
            .map_err(|e| {
                log::error!("[gifts] publishing {}: {e}", g.global_gift_id);
                bad(5, StatusCode::INTERNAL_SERVER_ERROR)
            })?;
        written += 1;
    }

    log::info!(
        "[gifts] published {written} gift(s) by {}: {summary}",
        body.published_by.as_deref().unwrap_or("unknown")
    );

    Ok(Json(GiftPublishResponse {
        applied: true,
        written,
        gift_ids: ids,
        summary,
    }))
}

// --- the giveaway ------------------------------------------------------------

#[derive(Debug, Clone, diesel::Queryable, diesel::Selectable, diesel::Insertable)]
#[diesel(table_name = crate::schema::free_for_all_runs)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct FreeForAllRun {
    pub id: Uuid,
    pub opens_at: i64,
    pub closes_at: i64,
    pub gems: i64,
    pub multiplier: i32,
    pub cadence: String,
    pub opened_by: Option<String>,
    pub note: Option<String>,
    pub created_at: i64,
}

/// The window live at `now`, if any.
///
/// Public because two callers need it: `announcements` builds the banner from it,
/// and `arena_economy` asks it whether a finished match earns Gems.
pub async fn open_run(conn: &mut AsyncPgConnection, now: i64) -> Option<FreeForAllRun> {
    use crate::schema::free_for_all_runs::dsl as r;
    r::free_for_all_runs
        .filter(r::opens_at.le(now))
        .filter(r::closes_at.gt(now))
        .order(r::opens_at.desc())
        .select(FreeForAllRun::as_select())
        .first(conn)
        .await
        .ok()
}

/// Claim this character's one payout for the open window, if there is one and
/// they have not already earned it. Returns the Gems to add to the match reward.
///
/// Called from inside the match-end transaction, so the grant row and the wallet
/// credit commit together: a crash between them would otherwise either pay twice
/// or mark someone paid who never was.
///
/// The `ON CONFLICT DO NOTHING` is what makes it safe under concurrency — two
/// matches ending at the same instant for the same character race on the primary
/// key, and exactly one insert reports a row.
pub async fn try_earn(
    conn: &mut AsyncPgConnection,
    character_id: Uuid,
    now: i64,
) -> Option<(Uuid, i64)> {
    let run = open_run(conn, now).await?;
    use crate::schema::free_for_all_grants::dsl as g;
    let inserted = diesel::insert_into(g::free_for_all_grants)
        .values((
            g::run_id.eq(run.id),
            g::character_id.eq(character_id),
            g::gems.eq(run.gems),
            g::granted_at.eq(now),
        ))
        .on_conflict((g::run_id, g::character_id))
        .do_nothing()
        .execute(conn)
        .await
        .ok()?;
    (inserted > 0).then_some((run.id, run.gems))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RunView {
    id: Uuid,
    opens_at: i64,
    closes_at: i64,
    gems: i64,
    multiplier: i32,
    cadence: String,
    opened_by: Option<String>,
    note: Option<String>,
    /// How many characters have earned it so far — the only honest measure of
    /// whether a giveaway actually reached anyone.
    earned_by: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannedView {
    opens_at: i64,
    closes_at: i64,
    gems: u64,
    multiplier: u32,
    /// Scheduled occurrences that went by without a window since the last one.
    missed: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StateResponse {
    now: i64,
    cadence: String,
    base_gems: u64,
    gems_currency_id: Uuid,
    start_hour_utc: i64,
    duration_secs: i64,
    runs: Vec<RunView>,
    planned: PlannedView,
    /// The window live right now, if any.
    open_run: Option<RunView>,
}

async fn load_runs(conn: &mut AsyncPgConnection) -> Result<Vec<FreeForAllRun>, BladeApiError> {
    use crate::schema::free_for_all_runs::dsl as r;
    r::free_for_all_runs
        .order(r::opens_at.desc())
        .limit(50)
        .select(FreeForAllRun::as_select())
        .load(conn)
        .await
        .map_err(|_| bad(4, StatusCode::INTERNAL_SERVER_ERROR))
}

async fn earned_count(conn: &mut AsyncPgConnection, run_id: Uuid) -> i64 {
    use crate::schema::free_for_all_grants::dsl as g;
    g::free_for_all_grants
        .filter(g::run_id.eq(run_id))
        .count()
        .get_result(conn)
        .await
        .unwrap_or(0)
}

async fn view(conn: &mut AsyncPgConnection, r: &FreeForAllRun) -> RunView {
    RunView {
        id: r.id,
        opens_at: r.opens_at,
        closes_at: r.closes_at,
        gems: r.gems,
        multiplier: r.multiplier,
        cadence: r.cadence.clone(),
        opened_by: r.opened_by.clone(),
        note: r.note.clone(),
        earned_by: earned_count(conn, r.id).await,
    }
}

#[derive(Debug, Deserialize)]
pub struct StateQuery {
    cadence: Option<String>,
    start_hour_utc: Option<i64>,
    duration_mins: Option<i64>,
}

fn window_from(start_hour_utc: Option<i64>, duration_mins: Option<i64>) -> Window {
    Window {
        start_hour_utc: start_hour_utc
            .unwrap_or(ffa::DEFAULT_START_HOUR_UTC)
            .clamp(0, 23),
        // At least a minute, at most a day: a zero-length window pays nobody and
        // a week-long one is not a giveaway.
        duration_secs: duration_mins
            .map(|m| m * 60)
            .unwrap_or(ffa::DEFAULT_DURATION_SECS)
            .clamp(60, ffa::DAY),
    }
}

/// `GET /…/api/dev/v1/free-for-all` — the ledger and what the next window would be.
#[get("/blades.bgs.services/api/dev/v1/free-for-all")]
pub async fn free_for_all_state(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<StateQuery>,
) -> Result<Json<StateResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| bad(5, StatusCode::SERVICE_UNAVAILABLE))?;

    let cadence = cadence_from_str(query.cadence.as_deref().unwrap_or("first_saturday"));
    let window = window_from(query.start_hour_utc, query.duration_mins);
    let now = now_secs();

    let runs = load_runs(&mut conn).await?;
    let last_opened = runs.iter().map(|r| r.opens_at).max();
    let plan = ffa::plan(cadence, window, now, last_opened);
    let missed = ffa::missed_since(cadence, last_opened, plan.opens_at);

    let mut views = Vec::with_capacity(runs.len());
    for r in &runs {
        views.push(view(&mut conn, r).await);
    }
    let open_now = match open_run(&mut conn, now).await {
        Some(r) => Some(view(&mut conn, &r).await),
        None => None,
    };

    Ok(Json(StateResponse {
        now,
        cadence: cadence_to_str(cadence).to_string(),
        base_gems: ffa::BASE_GEMS,
        gems_currency_id: GEMS,
        start_hour_utc: window.start_hour_utc,
        duration_secs: window.duration_secs,
        runs: views,
        planned: PlannedView {
            opens_at: plan.opens_at,
            closes_at: plan.closes_at,
            gems: plan.gems,
            multiplier: plan.multiplier,
            missed,
        },
        open_run: open_now,
    }))
}

/// `POST /…/api/dev/v1/free-for-all/open` request. **Dry run by default.**
///
/// Opening a window credits every player who fights in it, so the safe default is
/// to describe what would happen — the same convention the season rollover uses.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRequest {
    #[serde(default)]
    pub apply: bool,
    #[serde(default)]
    pub cadence: Option<String>,
    #[serde(default)]
    pub start_hour_utc: Option<i64>,
    #[serde(default)]
    pub duration_mins: Option<i64>,
    /// Override the computed payout. Left unset, the doubling rule decides.
    #[serde(default)]
    pub gems: Option<u64>,
    #[serde(default)]
    pub opened_by: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenResponse {
    pub applied: bool,
    pub run_id: Option<Uuid>,
    pub opens_at: i64,
    pub closes_at: i64,
    pub gems: u64,
    pub multiplier: u32,
    pub missed: u32,
    pub summary: String,
}

/// `POST /…/api/dev/v1/free-for-all/open` — schedule the next window.
///
/// Refuses a second window for the same occurrence, so a race between the console
/// and a scheduler cannot pay twice.
#[post("/blades.bgs.services/api/dev/v1/free-for-all/open")]
pub async fn free_for_all_open(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: Json<OpenRequest>,
) -> Result<Json<OpenResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let body = body.into_inner();
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| bad(5, StatusCode::SERVICE_UNAVAILABLE))?;

    let cadence = cadence_from_str(body.cadence.as_deref().unwrap_or("first_saturday"));
    let window = window_from(body.start_hour_utc, body.duration_mins);
    let now = now_secs();

    let runs = load_runs(&mut conn).await?;
    let last_opened = runs.iter().map(|r| r.opens_at).max();
    let plan = ffa::plan(cadence, window, now, last_opened);
    let missed = ffa::missed_since(cadence, last_opened, plan.opens_at);
    let gems = body.gems.unwrap_or(plan.gems);

    if runs.iter().any(|r| r.opens_at == plan.opens_at) {
        return Err(bad(2, StatusCode::CONFLICT));
    }

    let summary = format!(
        "Arena Giveaway: {} Gems (x{}) to every character who fights between {} and {}{}",
        gems,
        plan.multiplier,
        plan.opens_at,
        plan.closes_at,
        if missed > 0 {
            format!(" (catching up on {missed} missed)")
        } else {
            String::new()
        },
    );

    let mut run_id = None;
    if body.apply {
        use crate::schema::free_for_all_runs::dsl as r;
        let row = FreeForAllRun {
            id: Uuid::new_v4(),
            opens_at: plan.opens_at,
            closes_at: plan.closes_at,
            gems: gems as i64,
            multiplier: plan.multiplier as i32,
            cadence: cadence_to_str(cadence).to_string(),
            opened_by: body.opened_by.clone(),
            note: body.note.clone(),
            created_at: now,
        };
        run_id = Some(row.id);
        diesel::insert_into(r::free_for_all_runs)
            .values(&row)
            .execute(&mut conn)
            .await
            // A unique violation means somebody opened the same occurrence
            // between our read and our write. Conflict, not a 500.
            .map_err(|_| bad(2, StatusCode::CONFLICT))?;
    }

    Ok(Json(OpenResponse {
        applied: body.apply,
        run_id,
        opens_at: plan.opens_at,
        closes_at: plan.closes_at,
        gems,
        multiplier: plan.multiplier,
        missed,
        summary,
    }))
}

/// `POST /…/api/dev/v1/free-for-all/close` — end the live window early.
///
/// Moves `closes_at` to now. The run and its grant rows stay: they are the
/// record of who was paid, and deleting them would let the same characters earn
/// again on the next window opened at the same instant.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseRequest {
    #[serde(default)]
    pub apply: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseResponse {
    pub applied: bool,
    pub run_id: Option<Uuid>,
    pub closes_at: i64,
    pub earned_by: i64,
}

#[post("/blades.bgs.services/api/dev/v1/free-for-all/close")]
pub async fn free_for_all_close(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: Json<CloseRequest>,
) -> Result<Json<CloseResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let body = body.into_inner();
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| bad(5, StatusCode::SERVICE_UNAVAILABLE))?;
    let now = now_secs();

    let Some(run) = open_run(&mut conn, now).await else {
        return Err(bad(1, StatusCode::NOT_FOUND));
    };
    let earned_by = earned_count(&mut conn, run.id).await;

    if body.apply {
        use crate::schema::free_for_all_runs::dsl as r;
        diesel::update(r::free_for_all_runs.filter(r::id.eq(run.id)))
            .set(r::closes_at.eq(now))
            .execute(&mut conn)
            .await
            .map_err(|_| bad(3, StatusCode::INTERNAL_SERVER_ERROR))?;
    }

    Ok(Json(CloseResponse {
        applied: body.apply,
        run_id: Some(run.id),
        closes_at: now,
        earned_by,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_strings_round_trip() {
        for c in [Cadence::FirstSaturdayOfMonth, Cadence::EverySaturday] {
            assert_eq!(cadence_from_str(cadence_to_str(c)), c);
        }
    }

    #[test]
    fn an_unknown_cadence_falls_back_to_the_retail_one() {
        // A typo in a console query string must not silently switch the giveaway
        // to weekly, which would pay four times a month.
        assert_eq!(
            cadence_from_str("weekly-ish"),
            Cadence::FirstSaturdayOfMonth
        );
    }

    #[test]
    fn the_default_window_is_the_one_off_the_retail_banner() {
        let w = window_from(None, None);
        assert_eq!(w.duration_secs, 2 * ffa::HOUR, "10am-12pm ET is two hours");
        assert_eq!(w.start_hour_utc, ffa::DEFAULT_START_HOUR_UTC);
    }

    #[test]
    fn a_nonsense_window_is_clamped_rather_than_accepted() {
        // A zero-length window pays nobody and a week-long one is not a giveaway;
        // an out-of-range hour would push the window onto the wrong day.
        assert_eq!(window_from(Some(99), Some(0)).start_hour_utc, 23);
        assert_eq!(window_from(Some(-5), None).start_hour_utc, 0);
        assert_eq!(window_from(None, Some(0)).duration_secs, 60);
        assert_eq!(window_from(None, Some(999_999)).duration_secs, ffa::DAY);
    }

    #[test]
    fn an_override_row_becomes_the_gift_the_client_sees() {
        let row = GiftOverrideRow {
            gift_id: Uuid::from_u128(7),
            items: JsonDbWrapper(vec![GiftItem {
                item_template_id: GEMS,
                quantity: 200,
            }]),
            chests: JsonDbWrapper(Vec::new()),
            start_time: 100,
            end_time: 200,
            claim_count_limit: 3,
            description: Some("Sunset Gift".into()),
            updated_at: 0,
            updated_by: None,
        };
        let def = row.to_def();
        assert_eq!(def.global_gift_id, Uuid::from_u128(7));
        assert_eq!(def.claim_count_limit, 3);
        assert_eq!(def.items[0].quantity, 200);
        assert_eq!(def.items[0].item_template_id, GEMS);
    }

    #[test]
    fn a_negative_claim_limit_cannot_wrap_to_a_huge_one() {
        let row = GiftOverrideRow {
            gift_id: Uuid::from_u128(8),
            items: JsonDbWrapper(vec![]),
            chests: JsonDbWrapper(Vec::new()),
            start_time: 0,
            end_time: 0,
            claim_count_limit: -5,
            description: None,
            updated_at: 0,
            updated_by: None,
        };
        assert_eq!(row.to_def().claim_count_limit, 0);
    }
}

#[cfg(test)]
mod publish_tests {
    use super::*;

    fn gift(items: usize, chests: usize) -> GiftPublishItem {
        GiftPublishItem {
            global_gift_id: Uuid::new_v4(),
            items: (0..items)
                .map(|_| serde_json::from_str::<GiftItem>(
                    r#"{"itemTemplateId":"def810af-e9f5-4e23-9247-1edf391d82e1","quantity":1}"#,
                ).expect("a GiftItem the wire shape parses"))
                .collect(),
            chests: (0..chests)
                .map(|_| serde_json::from_str::<GiftChest>(r#"{"rarity":3}"#)
                    .expect("a GiftChest the wire shape parses"))
                .collect(),
            start_time: 0,
            end_time: 0,
            claim_count_limit: 1,
            description: None,
        }
    }

    /// A gift with neither items nor chests is a claim button that gives air.
    #[test]
    fn an_empty_gift_is_refused() {
        assert!(first_empty_gift(&[gift(0, 0)]).is_some());
        // …and it is found even when it is not the first in the batch, which is
        // the case a `gifts[0]`-only check would wave through.
        let batch = vec![gift(1, 0), gift(0, 0), gift(0, 1)];
        let found = first_empty_gift(&batch).expect("the empty one");
        assert_eq!(found, batch[1].global_gift_id);
    }

    /// CONTROL: a gift with contents of either kind is accepted, so the test
    /// above is about emptiness and not about the check refusing everything.
    #[test]
    fn a_gift_with_contents_is_accepted() {
        assert!(first_empty_gift(&[gift(1, 0)]).is_none());
        assert!(first_empty_gift(&[gift(0, 1)]).is_none());
        assert!(first_empty_gift(&[gift(2, 3)]).is_none());
        assert!(first_empty_gift(&[]).is_none());
    }

    /// A negative limit stored verbatim reads back as u64 — 18 quintillion free
    /// claims. The clamp is what stops one typo becoming unlimited loot.
    #[test]
    fn a_negative_claim_limit_clamps_to_zero() {
        assert_eq!(stored_claim_limit(-1), 0);
        assert_eq!(stored_claim_limit(i64::MIN), 0);
        // CONTROL: real limits pass through untouched.
        assert_eq!(stored_claim_limit(0), 0);
        assert_eq!(stored_claim_limit(1), 1);
        assert_eq!(stored_claim_limit(50), 50);
    }
}
