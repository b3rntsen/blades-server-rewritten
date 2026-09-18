//! Free for All — run the recurring gem giveaway, and the runtime gift overrides
//! that make it possible.
//!
//! The retail promotion (roughly the first Saturday of the month, 100 Gems to
//! everyone who showed up, doubled to 200 when Bethesda skipped a month) was a
//! manual operations job. This is the same thing as an endpoint, so the web
//! console can run it and the ledger says what was actually paid.
//!
//! Two pieces:
//!
//! * **`gift_overrides`** — gifts that live in the database. A row wins over
//!   `deploy/static/gifts.json`, and a row for an id the static file has never
//!   heard of simply IS a new gift. Without it, handing out anything means
//!   editing a bind-mounted file on the box and restarting the arena server,
//!   which is not something a console can do.
//! * **`free_for_all_runs`** — one row per giveaway actually opened. The doubling
//!   is derived from the gaps between these rows, so "we forgot last month" is a
//!   fact in the data rather than a flag someone has to remember to set.
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
use blades_lib::features::free_for_all::{self as ffa, Cadence};
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
            claim_count_limit: self.claim_count_limit.max(0) as u64,
            description: self.description.clone(),
        }
    }
}

/// The gift definition in force for `gift_id`: the database override if there is
/// one, otherwise the static catalogue.
///
/// Every `globalgifts` handler goes through here. A handler that read
/// `static_data.gifts` directly would serve the stale payload and the promotion
/// would silently not happen — which is the failure this whole module exists to
/// remove, so it must not be reintroduced one call site at a time.
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

/// Every gift id the server can serve — static catalogue plus anything the
/// database has introduced.
///
/// The client discovers gift ids at runtime, from `/announcements`: an entry's
/// `assetUrl` resolves to a manifest carrying a `GlobalGiftId`, and the Claim
/// button posts to that id. Retail leaned on this hard — captured characters
/// carry **311 distinct claimed gift ids** between them, far more than any
/// build could have baked in — which is why a run mints a fresh id rather than
/// re-pointing an old one.
async fn all_gift_ids(app_state: &ServerGlobal, conn: &mut AsyncPgConnection) -> Vec<Uuid> {
    use crate::schema::gift_overrides::dsl as go;
    let mut ids: Vec<Uuid> = app_state.static_data.gifts.keys().copied().collect();
    if let Ok(rows) = go::gift_overrides
        .select(go::gift_id)
        .load::<Uuid>(conn)
        .await
    {
        for id in rows {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids.sort();
    ids
}

#[derive(Debug, diesel::Queryable, diesel::Selectable, diesel::Insertable)]
#[diesel(table_name = crate::schema::free_for_all_runs)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct FreeForAllRun {
    pub id: Uuid,
    pub gift_id: Uuid,
    pub opens_at: i64,
    pub closes_at: i64,
    pub gems: i64,
    pub multiplier: i32,
    pub claim_limit: i64,
    pub cadence: String,
    pub opened_by: Option<String>,
    pub note: Option<String>,
    pub created_at: i64,
}

/// The run collectable at `now`, if any.
///
/// Public because `announcements` needs it: the banner is derived from the run
/// rather than written beside it, so an advert cannot outlive its gift or point
/// at one that was never created.
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RunView {
    id: Uuid,
    gift_id: Uuid,
    opens_at: i64,
    closes_at: i64,
    gems: i64,
    multiplier: i32,
    claim_limit: i64,
    cadence: String,
    opened_by: Option<String>,
    note: Option<String>,
}

impl From<&FreeForAllRun> for RunView {
    fn from(r: &FreeForAllRun) -> Self {
        RunView {
            id: r.id,
            gift_id: r.gift_id,
            opens_at: r.opens_at,
            closes_at: r.closes_at,
            gems: r.gems,
            multiplier: r.multiplier,
            claim_limit: r.claim_limit,
            cadence: r.cadence.clone(),
            opened_by: r.opened_by.clone(),
            note: r.note.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GiftView {
    gift_id: Uuid,
    items: Vec<GiftItem>,
    chests: Vec<GiftChest>,
    start_time: i64,
    end_time: i64,
    claim_count_limit: u64,
    description: Option<String>,
    /// True when a `gift_overrides` row is what the players are being served.
    overridden: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlannedView {
    opens_at: i64,
    closes_at: i64,
    gems: u64,
    multiplier: u32,
    /// Scheduled occurrences that went by without a run since the last one.
    missed: u32,
    /// True once `now` is inside the planned window — i.e. the run is overdue.
    due_now: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StateResponse {
    now: i64,
    cadence: String,
    base_gems: u64,
    gems_currency_id: Uuid,
    gifts: Vec<GiftView>,
    runs: Vec<RunView>,
    planned: PlannedView,
    /// The run currently collectable, if any.
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

/// `GET /…/api/dev/v1/free-for-all` — everything the console needs in one call:
/// the gift catalogue as players actually see it, the run ledger, and what the
/// next run would be if opened now.
#[get("/blades.bgs.services/api/dev/v1/free-for-all")]
pub async fn free_for_all_state(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<CadenceQuery>,
) -> Result<Json<StateResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| bad(5, StatusCode::SERVICE_UNAVAILABLE))?;

    let cadence = cadence_from_str(query.cadence.as_deref().unwrap_or("first_saturday"));
    let now = now_secs();

    let mut gifts = Vec::new();
    for id in all_gift_ids(&app_state, &mut conn).await {
        let overridden = {
            use crate::schema::gift_overrides::dsl as go;
            go::gift_overrides
                .filter(go::gift_id.eq(id))
                .select(go::gift_id)
                .first::<Uuid>(&mut conn)
                .await
                .is_ok()
        };
        if let Some(def) = effective_gift(&app_state, &mut conn, id).await {
            gifts.push(GiftView {
                gift_id: def.global_gift_id,
                items: def.items,
                chests: def.chests,
                start_time: def.start_time,
                end_time: def.end_time,
                claim_count_limit: def.claim_count_limit,
                description: def.description,
                overridden,
            });
        }
    }

    let runs = load_runs(&mut conn).await?;
    let last_opened = runs.iter().map(|r| r.opens_at).max();
    let plan = ffa::plan(cadence, now, last_opened);
    let missed = ffa::missed_since(cadence, last_opened, plan.opens_at);
    let open_run = runs
        .iter()
        .find(|r| now >= r.opens_at && now < r.closes_at)
        .map(RunView::from);

    Ok(Json(StateResponse {
        now,
        cadence: cadence_to_str(cadence).to_string(),
        base_gems: ffa::BASE_GEMS,
        gems_currency_id: GEMS,
        gifts,
        runs: runs.iter().map(RunView::from).collect(),
        planned: PlannedView {
            opens_at: plan.opens_at,
            closes_at: plan.closes_at,
            gems: plan.gems,
            multiplier: plan.multiplier,
            missed,
            due_now: ffa::is_open(&plan, now),
        },
        open_run,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CadenceQuery {
    cadence: Option<String>,
}

/// `POST /…/api/dev/v1/free-for-all/open` request. **Dry run by default.**
///
/// Opening a run credits every player who claims it, so the safe default is to
/// describe what would happen. `apply: true` is the deliberate act — the same
/// convention the season rollover uses.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRequest {
    /// Reuse a specific gift id instead of minting one. Only for repairing a
    /// run whose announcement went out but whose gift row did not land —
    /// normally leave it unset, because a fresh id per occurrence is what lets
    /// the same player collect again next month.
    #[serde(default)]
    pub gift_id: Option<Uuid>,
    #[serde(default)]
    pub apply: bool,
    #[serde(default)]
    pub cadence: Option<String>,
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
    pub gift_id: Uuid,
    pub opens_at: i64,
    pub closes_at: i64,
    pub gems: u64,
    pub multiplier: u32,
    pub missed: u32,
    /// The claim limit written onto the gift. It is one higher than before, which
    /// is what lets a character who already claimed this id claim it again.
    pub claim_limit: i64,
    pub summary: String,
}

/// `POST /…/api/dev/v1/free-for-all/open` — open the next giveaway.
///
/// Mints a gift worth `gems` Gems, claimable once per character inside a one-day
/// window, and records the run. The client finds it through `/announcements`,
/// which [`crate::announcements`] derives from the open run — so the gift and
/// the thing that advertises it cannot drift apart.
///
/// Refuses a second run for the same occurrence, so a race between the console
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
    let now = now_secs();

    let runs = load_runs(&mut conn).await?;
    let last_opened = runs.iter().map(|r| r.opens_at).max();
    let plan = ffa::plan(cadence, now, last_opened);
    let missed = ffa::missed_since(cadence, last_opened, plan.opens_at);
    let gems = body.gems.unwrap_or(plan.gems);

    // One giveaway per occurrence, whichever id it uses. The unique index is on
    // `(gift_id, opens_at)` and a minted id is always new, so this check — not
    // the index — is what stops two runs paying out on the same day.
    if runs.iter().any(|r| r.opens_at == plan.opens_at) {
        return Err(bad(2, StatusCode::CONFLICT));
    }

    // A fresh id per occurrence, the way retail did it. Claim counts are
    // permanent per (character, gift), so reusing an id would need its limit
    // raised every month and would hand a brand-new player a claim they never
    // earned; a new id gives everyone exactly one.
    let gift_id = body.gift_id.unwrap_or_else(Uuid::new_v4);
    let claim_limit = 1_i64;
    let summary = format!(
        "Free for All: {} Gems (x{}) as gift {} from {} to {}{}",
        gems,
        plan.multiplier,
        gift_id,
        plan.opens_at,
        plan.closes_at,
        if missed > 0 {
            format!(" (catching up on {missed} missed)")
        } else {
            String::new()
        },
    );

    if body.apply {
        use crate::schema::{free_for_all_runs::dsl as r, gift_overrides::dsl as go};

        let row = GiftOverrideRow {
            gift_id,
            items: JsonDbWrapper(vec![GiftItem {
                item_template_id: GEMS,
                quantity: gems,
            }]),
            chests: JsonDbWrapper(Vec::new()),
            start_time: plan.opens_at,
            end_time: plan.closes_at,
            claim_count_limit: claim_limit,
            description: Some("Free for All".to_string()),
            updated_at: now,
            updated_by: body.opened_by.clone(),
        };
        diesel::insert_into(go::gift_overrides)
            .values(&row)
            .on_conflict(go::gift_id)
            .do_update()
            .set((
                go::items.eq(&row.items),
                go::chests.eq(&row.chests),
                go::start_time.eq(row.start_time),
                go::end_time.eq(row.end_time),
                go::claim_count_limit.eq(row.claim_count_limit),
                go::description.eq(&row.description),
                go::updated_at.eq(row.updated_at),
                go::updated_by.eq(&row.updated_by),
            ))
            .execute(&mut conn)
            .await
            .map_err(|_| bad(3, StatusCode::INTERNAL_SERVER_ERROR))?;

        let run = FreeForAllRun {
            id: Uuid::new_v4(),
            gift_id,
            opens_at: plan.opens_at,
            closes_at: plan.closes_at,
            gems: gems as i64,
            multiplier: plan.multiplier as i32,
            claim_limit,
            cadence: cadence_to_str(cadence).to_string(),
            opened_by: body.opened_by.clone(),
            note: body.note.clone(),
            created_at: now,
        };
        diesel::insert_into(r::free_for_all_runs)
            .values(&run)
            .execute(&mut conn)
            .await
            // A unique-violation here means somebody else opened the same
            // occurrence between our read and our write. Conflict, not a 500.
            .map_err(|_| bad(2, StatusCode::CONFLICT))?;
    }

    Ok(Json(OpenResponse {
        applied: body.apply,
        gift_id,
        opens_at: plan.opens_at,
        closes_at: plan.closes_at,
        gems,
        multiplier: plan.multiplier,
        missed,
        claim_limit,
        summary,
    }))
}

/// `POST /…/api/dev/v1/free-for-all/close` — shut the current window early.
///
/// Sets the gift's `endTime` to now, which closes the window for anyone who has
/// not collected yet. The row stays: claim counts are permanent per character,
/// and a gift that vanishes from under a client mid-claim is a 404 on a button
/// the player was told to press.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseRequest {
    pub gift_id: Uuid,
    #[serde(default)]
    pub apply: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseResponse {
    pub applied: bool,
    pub gift_id: Uuid,
    pub end_time: i64,
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

    if body.apply {
        use crate::schema::gift_overrides::dsl as go;
        let updated = diesel::update(go::gift_overrides.filter(go::gift_id.eq(body.gift_id)))
            .set((go::end_time.eq(now), go::updated_at.eq(now)))
            .execute(&mut conn)
            .await
            .map_err(|_| bad(3, StatusCode::INTERNAL_SERVER_ERROR))?;
        if updated == 0 {
            return Err(bad(1, StatusCode::NOT_FOUND));
        }
    }

    Ok(Json(CloseResponse {
        applied: body.apply,
        gift_id: body.gift_id,
        end_time: now,
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
        // A typo in a console query string must not silently switch the
        // giveaway to weekly, which would pay four times a month.
        assert_eq!(
            cadence_from_str("weekly-ish"),
            Cadence::FirstSaturdayOfMonth
        );
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
            description: Some("Free for All".into()),
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
        // claim_count_limit is BIGINT in the database and u64 in GiftDef; a
        // hand-edited negative row must clamp to zero, not to 18 quintillion
        // free claims.
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
