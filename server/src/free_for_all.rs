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
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
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
