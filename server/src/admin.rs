//! Admin / management endpoints.
//!
//! Out-of-band operator endpoints that are **not** part of the Blades client
//! protocol. They're grouped in one module (rather than scattered among the
//! game services) so the management surface is easy to find and lock down:
//! every handler here is dev-token gated, never a game session.
//!
//! Auth: each endpoint requires an `Authorization: Bearer <token>` (or
//! `X-Import-Token: <token>`) header equal to the `ARENA_IMPORT_TOKEN` env var
//! captured at startup. If the env var is unset the admin surface is disabled
//! (503); a missing header is 401; a mismatched one is 403.
//!
//! Endpoints:
//!   * [`import_character`] — `POST /…/api/dev/v1/import-character` — seed a
//!     fully-formed, playable character straight into the `characters` table
//!     (and a backing `users` row if absent), bypassing the create-character
//!     flow. The capture -> server transform lives in the capture platform;
//!     this handler accepts the four `blades_lib` parts (`character`, `data`,
//!     `inventory`, `wallet`) directly.

use std::{collections::HashMap, sync::Arc};

use actix_web::{
    HttpRequest,
    get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::user_data::{
    CompleteCharacter, CompleteCharacterData, CompleteInventory, CompleteWallet,
    DungeonGeneratedData, DungeonGeneratedDataWithId, QuestWithId, UserAccount,
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper, insert_into};
use diesel_async::{AsyncConnection, RunQueryDsl, scoped_futures::ScopedFutureExt};
use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    arena::arena_season::{self, SeasonConfig},
    arena::matchmaker::{RecentTicketView, query_recent_matches},
    json_db::JsonDbWrapper,
    models::{CharacterDbAlone, CharacterDbEntry, QuestDbEntry, UserDBEntry},
    schema::{characters, quests, users},
};

// service id used in the BladeApiError envelope for this dev endpoint. Not a
// real Blades service id (those are client-facing); picked to be obviously
// out-of-band so import failures are easy to spot in logs.
const IMPORT_SERVICE_ID: u64 = 9001;

/// `bind-device` error codes inside `IMPORT_SERVICE_ID`. Named because the
/// endpoint now distinguishes four failures where it used to distinguish two,
/// and a bare integer at the call site says nothing about which.
///
/// `NO_CONNECTION` and `NO_SUCH_USER` are new; `DB_FAILED` (10) and `REFUSED`
/// (12) keep the numbers they already had on the wire, so nothing the capture
/// platform already handles changes meaning under it.
const BIND_DEVICE_DB_FAILED: u64 = 10;
const BIND_DEVICE_REFUSED: u64 = 12;
const BIND_DEVICE_NO_CONNECTION: u64 = 17;
const BIND_DEVICE_NO_SUCH_USER: u64 = 18;

/// Deserialize `quests[]` entry by entry, DROPPING any the schema cannot read
/// instead of failing the whole body.
///
/// WHY: report #59. One captured quest carried a negative `seed` where the
/// struct wanted `u64`, and serde rejected the entire `import-character`
/// request — so the player could not transfer his character at all, over one
/// field of one quest in a list that is a nicety. A quest we cannot parse
/// should cost that quest, never the character.
///
/// The strict behaviour is still right for the four `blades_lib` parts above:
/// a character whose gear or wallet will not deserialize is not importable in
/// any useful sense. It is wrong only for this list, which is additive.
///
/// Dropped entries are logged with the reason, so a schema drift shows up in
/// the log rather than silently shrinking people's quest logs.
fn quests_skipping_unparseable<'de, D>(de: D) -> Result<Vec<QuestWithId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<Value>::deserialize(de)?;
    let total = raw.len();
    let mut out = Vec::with_capacity(total);
    for v in raw {
        match serde_json::from_value::<QuestWithId>(v) {
            Ok(q) => out.push(q),
            Err(e) => warn!("[import] skipping an unreadable quest: {e}"),
        }
    }
    if out.len() != total {
        warn!(
            "[import] kept {}/{} quests; the rest could not be read",
            out.len(),
            total
        );
    }
    Ok(out)
}

/// The four `blades_lib` parts of a character, as sent on the wire (camelCase).
/// These mirror the JSONB columns of the `characters` table 1:1.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCharacterRequest {
    pub user_id: Uuid,
    pub character: CompleteCharacter,
    pub data: CompleteCharacterData,
    pub inventory: CompleteInventory,
    pub wallet: CompleteWallet,
    /// The character's own captured town (arbitrary JSON), served verbatim by
    /// `get_town`. Optional so older payloads / fresh imports still deserialize;
    /// when absent the existing stored town (if any) is left untouched.
    #[serde(default)]
    pub town: Option<Value>,
    /// The character's captured story quests — the `quests[]` array of a retail
    /// `/quests` response. Without these an imported character arrives with an
    /// empty quest table, so `get_quests` returns `quests: []` and the in-game
    /// quest map has nothing to draw (report #58). Job rows are not sent: those
    /// are rolled server-side per reset window.
    #[serde(default, deserialize_with = "quests_skipping_unparseable")]
    pub quests: Vec<QuestWithId>,
    /// Dungeon bodies for the quests above, matched by `questId`. A quest whose
    /// dungeon was never generated in the capture simply has no entry here.
    #[serde(default)]
    pub dungeon_generated_data_list: Vec<DungeonGeneratedDataWithId>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCharacterResponse {
    pub character_id: Uuid,
    pub user_id: Uuid,
    /// true if a brand-new character row was inserted; false if an existing
    /// row for this `userId` was overwritten.
    pub created: bool,
}

/// Pull the dev token out of the request, checking `Authorization: Bearer ...`
/// first and falling back to `X-Import-Token`.
fn extract_import_token(req: &HttpRequest) -> Option<String> {
    if let Some(value) = req.headers().get("Authorization") {
        if let Ok(value) = value.to_str() {
            if let Some(token) = value.strip_prefix("Bearer ") {
                return Some(token.trim().to_string());
            }
        }
    }
    if let Some(value) = req.headers().get("X-Import-Token") {
        if let Ok(value) = value.to_str() {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Validate the dev token against the one captured at startup. Returns the
/// appropriate `BladeApiError` on failure so the caller can `?` it.
pub(crate) fn check_import_token(app_state: &ServerGlobal, req: &HttpRequest) -> Result<(), BladeApiError> {
    // No token configured -> endpoint disabled.
    let expected = match app_state.arena_import_token.as_deref() {
        Some(token) if !token.is_empty() => token,
        _ => return Err(BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, IMPORT_SERVICE_ID, 1)),
    };

    match extract_import_token(req) {
        // constant-time-ish: lengths differ -> mismatch; otherwise compare.
        Some(provided) if provided == expected => Ok(()),
        Some(_) => Err(BladeApiError::new(StatusCode::FORBIDDEN, IMPORT_SERVICE_ID, 2)),
        None => Err(BladeApiError::new(StatusCode::UNAUTHORIZED, IMPORT_SERVICE_ID, 3)),
    }
}

#[post("/api/dev/v1/import-character")]
pub async fn import_character(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<ImportCharacterRequest>,
) -> Result<Json<ImportCharacterResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;

    let body = body.into_inner();
    let user_id = body.user_id;

    let mut conn = app_state.db_pool.get().await.unwrap();

    let response = conn
        .transaction::<_, BladeApiError, _>(|mut conn| {
            async move {
                // 1. Ensure a backing `users` row exists (characters.user_id is a
                //    NOT NULL FK -> users.id). If absent, insert a minimal user:
                //    a random secret_id and an empty UserAccount (no device ids).
                //    We never overwrite an existing user row here.
                let existing_user: i64 = users::table
                    .filter(users::id.eq(user_id))
                    .count()
                    .get_result(&mut conn)
                    .await?;

                if existing_user == 0 {
                    insert_into(users::table)
                        .values(UserDBEntry {
                            id: user_id,
                            secret_id: Uuid::new_v4(),
                            data: JsonDbWrapper(UserAccount::new_random()),
                        })
                        .execute(&mut conn)
                        .await?;
                }

                // 2. Upsert the character row. `characters` has a UNIQUE(user_id)
                //    constraint (one char per user), so look up the existing row
                //    (locking it) and either overwrite its four JSONB columns or
                //    insert a fresh row with a new id.
                let existing: Option<CharacterDbAlone> = characters::table
                    .filter(characters::user_id.eq(user_id))
                    .select(CharacterDbAlone::as_select())
                    .for_update()
                    .load(&mut conn)
                    .await?
                    .into_iter()
                    .next();

                let (character_id, created) = match existing {
                    Some(row) => (row.id, false),
                    None => (Uuid::new_v4(), true),
                };

                let entry = CharacterDbEntry {
                    id: character_id,
                    user_id,
                    character: JsonDbWrapper(body.character),
                    data: JsonDbWrapper(body.data),
                    wallet: JsonDbWrapper(body.wallet),
                    inventory: JsonDbWrapper(body.inventory),
                    town: body.town.map(JsonDbWrapper),
                };

                if created {
                    insert_into(characters::table)
                        .values(&entry)
                        .execute(&mut conn)
                        .await?;
                } else {
                    // Overwrite all four payload columns of the existing row.
                    diesel::update(characters::table)
                        .filter(characters::id.eq(character_id))
                        .set((
                            characters::character.eq(entry.character),
                            characters::data.eq(entry.data),
                            characters::wallet.eq(entry.wallet),
                            characters::inventory.eq(entry.inventory),
                        ))
                        .execute(&mut conn)
                        .await?;
                    // Town is overwritten only when the payload carries one, so a
                    // re-import without a captured town doesn't wipe a good one.
                    if let Some(town) = entry.town {
                        diesel::update(characters::table)
                            .filter(characters::id.eq(character_id))
                            .set(characters::town.eq(town))
                            .execute(&mut conn)
                            .await?;
                    }
                }

                // 3. Seed the character's captured story quests. Before this an
                //    imported character had no `quests` rows at all, so
                //    `get_quests` returned `quests: []` and the quest map was
                //    empty for every transferred player (report #58).
                //
                //    `do_nothing` on conflict: a re-import restores what the
                //    capture holds without clobbering progress the player has
                //    made on the live server since. Same reasoning as `town`
                //    above, and the same conflict target the job upsert in
                //    `quest.rs` uses.
                if !body.quests.is_empty() {
                    let mut dungeons: HashMap<Uuid, DungeonGeneratedData> = body
                        .dungeon_generated_data_list
                        .into_iter()
                        .map(|d| (d.quest_id, d.inner))
                        .collect();

                    for quest in body.quests {
                        let entry = QuestDbEntry {
                            id: quest.quest_id,
                            character_id,
                            info: JsonDbWrapper(quest.quest),
                            generated_data: JsonDbWrapper(dungeons.remove(&quest.quest_id)),
                            dungeon_state: None,
                        };
                        insert_into(quests::table)
                            .values(&entry)
                            .on_conflict((quests::id, quests::character_id))
                            .do_nothing()
                            .execute(&mut conn)
                            .await?;
                    }
                }

                Ok(ImportCharacterResponse {
                    character_id,
                    user_id,
                    created,
                })
            }
            .scope_boxed()
        })
        .await?;

    Ok(Json(response))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentMatchesQuery {
    #[serde(default)]
    user_id: Option<Uuid>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /…/api/dev/v1/recent-matches?userId=<uuid>&limit=<n>` — the most recent
/// matchmaking tickets (newest first), so the web /arena page can confirm a
/// user's match request registered + show recent arena activity. Dev-token
/// gated. `userId` only sets the per-row `mine` flag (the list is server-wide).
/// Durable: backed by the `arena_matches` table, so it survives restarts (#NB-3).
#[get("/api/dev/v1/recent-matches")]
pub async fn recent_matches(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<RecentMatchesQuery>,
) -> Result<Json<Vec<RecentTicketView>>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let q = query.into_inner();
    let limit = q.limit.unwrap_or(25).min(100);
    Ok(Json(
        query_recent_matches(&app_state.db_pool, limit as i64, q.user_id).await,
    ))
}

// ---------------------------------------------------------------------------
// Per-player claim link: bind a device's anon `deviceId` to a Transfer'd
// character's user, and list recently-seen devices for the web claim UI. Both
// dev-token gated (same as import). The device_bindings table + the anon_log_in
// lookup are in migration 2026-06-08_add_device_bindings / authentification.rs.
// Raw SQL (sql_query) avoids a timestamp-typed diesel schema.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindDeviceRequest {
    pub device_id: String,
    pub user_id: Uuid,
    /// Every WireGuard peer IP the capture platform has allocated to the
    /// claimant, as the capture platform knows it. Optional on the wire, and
    /// the reason it is optional is deploy ordering — see `BIND_DEVICE_SQL`.
    #[serde(default)]
    pub own_wg_ips: Option<Vec<String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindDeviceResponse {
    pub device_id: String,
    pub user_id: Uuid,
}

/// Claim a device, but never TAKE one.
///
/// The guard is the `WHERE` on `DO UPDATE`. The upsert may create a row, adopt
/// an unclaimed one (`user_id IS NULL` — the state `anon_log_in` leaves a device
/// in the first time it is seen), or re-affirm one the same user already holds.
/// It may not move a binding that belongs to somebody else, and when it declines
/// it leaves that row exactly as it was.
///
/// This has to be ONE statement. A `SELECT` to check the owner followed by an
/// `UPDATE` is two round-trips with a gap in between, and two claims racing
/// through that gap is the bug again — so the check lives inside the write,
/// where the row is already locked. Postgres reports the decline as zero rows
/// affected, which is what `bind_device` turns into a 409.
///
/// `EXCLUDED.user_id` rather than a second `$2` so the claimant is named once;
/// the two are the same value.
///
/// WHAT THE FIRST FIX MISSED
///
/// The guard above only refuses to MOVE a binding. An UNCLAIMED row
/// (`user_id IS NULL`) it hands to whoever asks first — and that is a real
/// breach, not a theoretical one, because `anon_log_in` creates exactly that
/// row the first time it sees any device. A player launches the game, the row
/// appears unclaimed, and somebody else claims it before they do; their client
/// then logs into the claimant's account on next launch. Ten of seventy rows in
/// production got there this way, across nine victims, after the first fix
/// shipped.
///
/// The row carries the missing evidence. `source_wg_ip` (migration
/// 2026-06-21-000000-0000_device_bindings_wg_ip) is the WireGuard peer IP the
/// device was seen on, and the capture platform ALLOCATED that IP, so it knows
/// whose it is. This server does not — the allocation table lives in the
/// capture platform's SQLite, not here — so the claimant's own peer IPs arrive
/// as `$3` and the comparison happens where the row is locked.
///
/// The rule, in the `WHERE`:
///   - re-affirming a binding you already hold always works (`user_id =
///     EXCLUDED.user_id`), whatever `source_wg_ip` says. The transfer flow
///     re-binds every one of a user's peers on each import, so this runs
///     constantly, and refusing it would break imports without protecting
///     anyone — nothing is acquired.
///   - ACQUIRING an unclaimed row additionally requires that its
///     `source_wg_ip` is unrecorded, or is one of the claimant's own peer IPs.
///     A device last seen on somebody else's tunnel is refused.
///
/// `$3 IS NULL` — the caller sent no peer list at all — falls back to the old
/// rule. That is a deploy ramp, not a resting state: the arena server and the
/// capture platform's web container deploy independently and in either order,
/// so a fail-closed `$3` would 409 every claim and every auto-bind in the
/// window where this build is live and the web build is not. The web side
/// always sends the list (empty array when the claimant has no peers, which is
/// fail-closed: `= ANY('{}')` is false, so every attributable row is refused).
/// Tighten this to `NOT NULL` once both halves are in production.
pub(crate) const BIND_DEVICE_SQL: &str =
    "INSERT INTO device_bindings (device_id, user_id, bound_at, last_seen) \
     VALUES ($1, $2, now(), now()) \
     ON CONFLICT (device_id) DO UPDATE SET user_id = EXCLUDED.user_id, bound_at = now() \
     WHERE device_bindings.user_id = EXCLUDED.user_id \
        OR (device_bindings.user_id IS NULL \
            AND ($3::text[] IS NULL \
                 OR device_bindings.source_wg_ip IS NULL \
                 OR device_bindings.source_wg_ip = ANY($3::text[])))";

/// `POST /…/api/dev/v1/bind-device` — bind a device to a user (the per-player
/// claim link); after this the device's `auth/anon` logs in as that user.
///
/// Claiming an unbound device works, and re-claiming your own is idempotent.
/// Claiming a device that already belongs to a DIFFERENT user is refused with
/// `409 Conflict` and writes nothing — and so is claiming an UNBOUND device
/// last seen on a WireGuard peer IP that is not the claimant's. See
/// `BIND_DEVICE_SQL`.
///
/// Why refusing matters: `device_id` is the WireGuard peer IP, and `anon_log_in`
/// (authentification.rs) resolves a device to its bound user. Moving somebody
/// else's binding therefore does not merely mislabel a row — it re-points their
/// game client at the claimant's account on its next launch.
/// Turn a failure of `BIND_DEVICE_SQL` into an HTTP envelope — and say what it
/// WAS first.
///
/// The previous `.map_err(|_| …500…)` discarded the error object entirely. Nine
/// production 500s in one evening (and more across the two days before) left no
/// message, no constraint name and no offending key anywhere in this server's
/// log; the only record of the cause was Postgres's own log inside the
/// `arena-db` container, which nobody reads. Whatever else this function does,
/// it must not fail silently again.
///
/// The classification that matters is `ForeignKeyViolation`.
/// `device_bindings.user_id` references `users(id)`, so a claim naming a user
/// this server has never heard of is rejected by the DATABASE, not by the
/// guard — and that is a bad request, not a server fault. It gets `404` so the
/// caller can tell "you named a user that does not exist" apart from "we
/// broke", and so it stops paging as a 5xx.
///
/// On SQLSTATE: diesel 2.3's `DatabaseErrorInformation` does not expose the raw
/// five-character code. `DatabaseErrorKind` is diesel's own classification OF
/// that code and is logged in its place; `details()` carries Postgres's DETAIL
/// line, which for this constraint names the offending key —
/// `Key (user_id)=(…) is not present in table "users"`. That is precisely the
/// diagnostic that was missing.
pub(crate) fn map_bind_device_error(e: &diesel::result::Error) -> BladeApiError {
    use diesel::result::{DatabaseErrorKind, Error as DieselError};

    if let DieselError::DatabaseError(kind, info) = e {
        // Logged for EVERY database rejection, classified or not: the
        // unclassified ones are exactly the ones we would next be blind to.
        warn!(
            "bind-device: database rejected the bind: kind={:?} message={:?} detail={:?} table={:?} constraint={:?}",
            kind,
            info.message(),
            info.details(),
            info.table_name(),
            info.constraint_name(),
        );
        if matches!(kind, DatabaseErrorKind::ForeignKeyViolation) {
            return BladeApiError::new(
                StatusCode::NOT_FOUND,
                IMPORT_SERVICE_ID,
                BIND_DEVICE_NO_SUCH_USER,
            );
        }
    } else {
        warn!("bind-device: the bind statement failed: {e}");
    }

    BladeApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        IMPORT_SERVICE_ID,
        BIND_DEVICE_DB_FAILED,
    )
}

#[post("/api/dev/v1/bind-device")]
pub async fn bind_device(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<BindDeviceRequest>,
) -> Result<Json<BindDeviceResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let body = body.into_inner();
    // Was `.unwrap()`. A pool that cannot hand out a connection is a real
    // operational state — restarting database, exhausted pool — and panicking
    // on it kills the worker instead of answering the request.
    let mut conn = app_state.db_pool.get().await.map_err(|e| {
        log::error!("bind-device: could not acquire a database connection: {e}");
        BladeApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            IMPORT_SERVICE_ID,
            BIND_DEVICE_NO_CONNECTION,
        )
    })?;
    let affected = diesel::sql_query(BIND_DEVICE_SQL)
        .bind::<diesel::sql_types::Text, _>(body.device_id.clone())
        .bind::<diesel::sql_types::Uuid, _>(body.user_id)
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Array<diesel::sql_types::Text>>, _>(
            body.own_wg_ips.clone(),
        )
        .execute(&mut conn)
        .await
        .map_err(|e| map_bind_device_error(&e))?;

    if affected == 0 {
        // Either the row is held by another user, or it is unclaimed but was
        // seen on a peer IP that is not the claimant's. Nothing was written.
        // Logged because a refusal is a security-relevant event and, until
        // now, an indistinguishable one: 409s and 500s looked identical in the
        // access log and neither said who or what.
        warn!(
            "bind-device: guard declined the claim of {:?} by {} (nothing written)",
            body.device_id, body.user_id
        );
        return Err(BladeApiError::new(
            StatusCode::CONFLICT,
            IMPORT_SERVICE_ID,
            BIND_DEVICE_REFUSED,
        ));
    }

    Ok(Json(BindDeviceResponse {
        device_id: body.device_id,
        user_id: body.user_id,
    }))
}

/// One recently-seen device, for the claim UI (most-recent first).
#[derive(Serialize, diesel::QueryableByName)]
#[serde(rename_all = "camelCase")]
pub struct RecentDevice {
    #[diesel(sql_type = diesel::sql_types::Text)]
    pub device_id: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    pub user_id: Option<Uuid>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    pub platform: Option<String>,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub age_seconds: i64,
    /// The WireGuard peer IP this device was last seen on, or NULL if it was
    /// never seen over a tunnel we recorded.
    ///
    /// This server cannot say anything about who that IP belongs to — the
    /// allocation table is the capture platform's. Returning it is what lets
    /// the capture platform decide, and it is only ever returned for rows the
    /// caller may already act on (unclaimed, or already theirs), so it
    /// discloses no third party's tunnel.
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    pub source_wg_ip: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentDevicesQuery {
    /// The arena user asking. Devices already bound to somebody ELSE are not
    /// theirs to see, so the answer is scoped to this id.
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// The claim list, scoped to what the caller may act on.
///
/// A device already bound to another player is neither claimable (see
/// `BIND_DEVICE_SQL`) nor any of the caller's business, so it is not returned at
/// all. Filtering in the query rather than at the caller means the other
/// player's `user_id` never crosses the wire, which matters because the web app
/// forwards this list to a browser.
///
/// Two rows qualify:
///   - `user_id IS NULL` — unclaimed, so claimable by anyone. This is the whole
///     legitimate flow: launch the game, then claim the device you just
///     launched, which nobody owns yet. Dropping these would leave the feature
///     with nothing to show.
///   - `user_id = $1` — already the caller's, shown so the UI can say "yours".
///
/// `$1` is `NULL` when the caller sends no `userId`. `user_id = NULL` is never
/// true in SQL, so that case degrades to unclaimed-only: still useful, still
/// leaks nothing. Fail-closed on purpose — an older web build that has not
/// learned to send `userId` yet loses the "yours" rows, not its privacy.
/// `source_wg_ip` rides along because "unclaimed" is not the same as
/// "unowned". An unclaimed row created by `anon_log_in` still records the
/// tunnel the device was seen on, and that tunnel was allocated by the capture
/// platform to somebody. Only the capture platform can make that judgement, so
/// the column has to reach it — without it, its claim rule can only ever say
/// "nobody has claimed this yet", which is precisely the hole this closes.
pub(crate) const RECENT_DEVICES_SQL: &str = "SELECT device_id, user_id, platform, \
     CAST(EXTRACT(epoch FROM (now() - last_seen)) AS BIGINT) AS age_seconds, \
     source_wg_ip \
     FROM device_bindings \
     WHERE user_id IS NULL OR user_id = $1 \
     ORDER BY last_seen DESC LIMIT 50";

/// `GET /…/api/dev/v1/recent-devices?userId=<uuid>` — the devices this player
/// may claim: unclaimed ones, plus the ones already theirs. Never another
/// player's. See `RECENT_DEVICES_SQL`.
#[get("/api/dev/v1/recent-devices")]
pub async fn recent_devices(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<RecentDevicesQuery>,
) -> Result<Json<Vec<RecentDevice>>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let mut conn = app_state.db_pool.get().await.unwrap();
    let rows = diesel::sql_query(RECENT_DEVICES_SQL)
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(query.into_inner().user_id)
        .get_results::<RecentDevice>(&mut conn)
        .await
        .map_err(|_| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 11))?;
    Ok(Json(rows))
}


// ------------------------------------------------------------ arena season

/// `POST /…/api/dev/v1/arena-season-rollover` request.
///
/// **Defaults to a dry run.** The rollover zeroes every player's trophies, so it
/// only writes when the caller explicitly says `"apply": true` — a missing or
/// mistyped field reports what would happen and changes nothing.
#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct SeasonRolloverRequest {
    /// Write the changes. Absent or false = report only.
    #[serde(default)]
    pub apply: bool,
    /// Roll into this season id instead of the one this build calls current.
    /// Only useful for re-running an older rollover; normally omitted.
    #[serde(default)]
    pub season_id: Option<Uuid>,
}

/// What the rollover did (or would do).
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SeasonRolloverResponse {
    /// False when this was a dry run.
    pub applied: bool,
    /// The season everyone was rolled into.
    pub season_id: Uuid,
    /// Its human-facing number.
    pub season_number: u32,
    /// Characters examined.
    pub characters_seen: usize,
    /// Characters whose counters were (or would be) zeroed.
    pub characters_reset: usize,
    /// Of those, how many had a previous season to file into `pvpSeasonHistory`.
    pub characters_archived: usize,
    /// Characters already in this season — left untouched.
    pub characters_already_current: usize,
    /// Rows whose `character` JSONB would not deserialize; skipped, never written.
    pub characters_unreadable: usize,
    /// The largest standing that was archived, as a sanity line for the operator.
    pub highest_archived_trophies: i64,
}

/// One `characters` row, narrowed to what the rollover touches.
#[derive(diesel::QueryableByName)]
struct SeasonRolloverRow {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Jsonb)]
    character: Value,
}

/// `POST /…/api/dev/v1/arena-season-rollover` — close the season every character
/// is in and open the current one.
///
/// For each character: archive its live PvP block into `pvpSeasonHistory` under
/// the season it was in, zero every live PvP counter, and stamp the new
/// `pvpSeasonId`. See `arena::arena_season` for the capture evidence behind that
/// behaviour, and `docs/arena-season-model.md` for how to run this.
///
/// Idempotent by construction: a character already stamped with the target
/// season is skipped, so re-running after a partial failure resumes rather than
/// wiping the players it already moved.
#[post("/api/dev/v1/arena-season-rollover")]
pub async fn arena_season_rollover(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: Option<web::Json<SeasonRolloverRequest>>,
) -> Result<Json<SeasonRolloverResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let body = body.map(|b| b.into_inner()).unwrap_or_default();

    let season: &SeasonConfig = match body.season_id {
        Some(id) => arena_season::SEASONS
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, IMPORT_SERVICE_ID, 12))?,
        None => arena_season::season_at(arena_season::now_unix())
            .or_else(|| arena_season::SEASONS.last())
            .ok_or_else(|| {
                BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, IMPORT_SERVICE_ID, 13)
            })?,
    };

    let mut conn = app_state.db_pool.get().await.unwrap();
    let rows: Vec<SeasonRolloverRow> =
        diesel::sql_query("SELECT id, character FROM characters ORDER BY id")
            .get_results(&mut conn)
            .await
            .map_err(|e| {
                warn!("season rollover: could not read characters: {e}");
                BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 14)
            })?;

    let mut resp = SeasonRolloverResponse {
        applied: body.apply,
        season_id: season.id,
        season_number: season.number,
        characters_seen: rows.len(),
        characters_reset: 0,
        characters_archived: 0,
        characters_already_current: 0,
        characters_unreadable: 0,
        highest_archived_trophies: 0,
    };

    for row in rows {
        let mut ch: CompleteCharacter = match serde_json::from_value(row.character.clone()) {
            Ok(c) => c,
            Err(e) => {
                warn!("season rollover: character {} does not deserialize: {e}", row.id);
                resp.characters_unreadable += 1;
                continue;
            }
        };
        let standing = ch.pvp_trophies;
        let outcome = arena_season::roll_character_into(&mut ch, season);
        if !outcome.reset {
            resp.characters_already_current += 1;
            continue;
        }
        resp.characters_reset += 1;
        if outcome.archived_under.is_some() {
            resp.characters_archived += 1;
            resp.highest_archived_trophies = resp.highest_archived_trophies.max(standing);
        }

        if !body.apply {
            continue;
        }
        let updated = serde_json::to_value(&ch).map_err(|e| {
            warn!("season rollover: character {} does not serialize: {e}", row.id);
            BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 15)
        })?;
        diesel::sql_query("UPDATE characters SET character = $1 WHERE id = $2")
            .bind::<diesel::sql_types::Jsonb, _>(updated)
            .bind::<diesel::sql_types::Uuid, _>(row.id)
            .execute(&mut conn)
            .await
            .map_err(|e| {
                warn!("season rollover: write failed for character {}: {e}", row.id);
                BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, IMPORT_SERVICE_ID, 16)
            })?;
    }

    log::info!(
        "arena season rollover ({}) into season {} (#{}) — seen {}, reset {}, archived {}, \
         already current {}, unreadable {}",
        if body.apply { "APPLIED" } else { "dry run" },
        resp.season_id,
        resp.season_number,
        resp.characters_seen,
        resp.characters_reset,
        resp.characters_archived,
        resp.characters_already_current,
        resp.characters_unreadable,
    );
    Ok(Json(resp))
}

#[cfg(test)]
mod tests {
    // --- season rollover: the dry-run default ------------------------------
    //
    // `arena_season_rollover` zeroes every player's trophies. Its only
    // safeguard against doing that by accident is that `apply` must be
    // *explicitly* true — the handler's `if !body.apply { continue; }` sits
    // between the plan and the UPDATE. The doc comment promises that "a
    // missing or mistyped field reports what would happen and changes
    // nothing"; these tests hold that promise to account, because the cost of
    // it silently becoming false is every player's standing.
    //
    // Scope: this covers the request half — how `apply` is decoded. The
    // handler's guard itself needs a live DB and is not exercised here.
    mod season_rollover_defaults {
        use super::super::SeasonRolloverRequest;

        #[test]
        fn an_empty_body_is_a_dry_run() {
            let r: SeasonRolloverRequest = serde_json::from_str("{}").unwrap();
            assert!(!r.apply, "POST with `{{}}` must not write");
        }

        #[test]
        fn an_absent_body_is_a_dry_run() {
            // The handler does `body.map(..).unwrap_or_default()`, so a
            // bodyless POST lands on Default.
            assert!(
                !SeasonRolloverRequest::default().apply,
                "a bodyless POST must not write"
            );
        }

        #[test]
        fn a_mistyped_apply_field_is_a_dry_run() {
            // The exact footgun the doc comment names: someone types `aply`
            // (or `Apply`, or `apply_now`) and expects a wipe. They must get
            // a report instead. If anyone adds `deny_unknown_fields` this
            // becomes a 400 — also safe, but this test would then need
            // rewriting rather than deleting.
            for typo in [r#"{"aply":true}"#, r#"{"Apply":true}"#, r#"{"apply_now":true}"#] {
                let r: SeasonRolloverRequest = serde_json::from_str(typo)
                    .unwrap_or_else(|e| panic!("{typo} should parse or 400, got {e}"));
                assert!(!r.apply, "{typo} must not arm the wipe");
            }
        }

        #[test]
        fn apply_true_is_the_only_thing_that_arms_it() {
            // The positive control. Without this the three tests above would
            // still pass if `apply` were hard-wired to false, and the
            // endpoint would be silently inert rather than safe.
            let r: SeasonRolloverRequest = serde_json::from_str(r#"{"apply":true}"#).unwrap();
            assert!(r.apply, "an explicit `apply: true` must still work");
        }

        #[test]
        fn apply_is_not_coerced_from_a_truthy_string_or_number() {
            // serde is strict about this, but the assertion pins it: a config
            // templating layer that stringifies booleans must fail loudly,
            // not arm a wipe.
            for loose in [r#"{"apply":"true"}"#, r#"{"apply":1}"#] {
                assert!(
                    serde_json::from_str::<SeasonRolloverRequest>(loose).is_err(),
                    "{loose} must be rejected, never read as true"
                );
            }
        }
    }

    use super::*;
    use blades_lib::user_data::{
        CompleteCharacter, CompleteCharacterData, CompleteInventory, CompleteWallet,
    };

    /// The wire contract: a representative camelCase JSON body deserializes
    /// into `ImportCharacterRequest` with the four `blades_lib` parts intact.
    ///
    /// We build the body by serializing the library defaults (so the test
    /// stays in lockstep with the real serde shapes of each part) and only
    /// hand-write the `userId` and a couple of character fields we then assert.
    #[test]
    fn import_request_deserializes_from_wire_json() {
        let user_id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();

        let mut character = CompleteCharacter::default();
        character.name = "DevSeed".to_string();
        character.level = 42;

        let body = serde_json::json!({
            "userId": user_id,
            "character": character,
            "data": CompleteCharacterData::default(),
            "inventory": {
                "backpack": serde_json::to_value(blades_lib::user_data::Backpack::default()).unwrap(),
                "loadout": serde_json::to_value(blades_lib::user_data::Loadout::default()).unwrap(),
                "treasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "overflowTreasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "backpackVersion": 1,
                "treasuryVersion": 0,
            },
            "wallet": CompleteWallet::default(),
        });

        let parsed: ImportCharacterRequest =
            serde_json::from_value(body).expect("representative import body must deserialize");

        assert_eq!(parsed.user_id, user_id);
        assert_eq!(parsed.character.name, "DevSeed");
        assert_eq!(parsed.character.level, 42);
        // sanity: the other parts round-tripped into their concrete types.
        assert_eq!(parsed.inventory.backpack_version, 1);
        let _: CompleteInventory = parsed.inventory;
        let _: CompleteWallet = parsed.wallet;
        let _: CompleteCharacterData = parsed.data;
    }

    /// `new-flags` is renamed (dash, not camelCase). Make sure a body using the
    /// real key deserializes and round-trips through `CompleteCharacterData`.
    #[test]
    fn data_part_accepts_new_flags_dash_key() {
        let body = serde_json::json!({
            "userId": "11111111-2222-3333-4444-555555555555",
            "character": CompleteCharacter::default(),
            "data": {
                "customization": { "CharacterUID": "abc" },
                "new-flags": { "seen_intro": true },
                "dialog": {}
            },
            "inventory": {
                "backpack": serde_json::to_value(blades_lib::user_data::Backpack::default()).unwrap(),
                "loadout": serde_json::to_value(blades_lib::user_data::Loadout::default()).unwrap(),
                "treasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "overflowTreasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "backpackVersion": 1,
                "treasuryVersion": 0,
            },
            "wallet": CompleteWallet::default(),
        });

        let parsed: ImportCharacterRequest =
            serde_json::from_value(body).expect("body with new-flags must deserialize");
        assert_eq!(parsed.data.new_flags["seen_intro"], serde_json::json!(true));
    }

    /// The inert parts of an import body, so the quest tests below show only
    /// what they are about.
    fn body_without_quests() -> serde_json::Value {
        serde_json::json!({
            "userId": "11111111-2222-3333-4444-555555555555",
            "character": CompleteCharacter::default(),
            "data": CompleteCharacterData::default(),
            "inventory": {
                "backpack": serde_json::to_value(blades_lib::user_data::Backpack::default()).unwrap(),
                "loadout": serde_json::to_value(blades_lib::user_data::Loadout::default()).unwrap(),
                "treasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "overflowTreasury": serde_json::to_value(blades_lib::user_data::Treasury::default()).unwrap(),
                "backpackVersion": 1,
                "treasuryVersion": 0,
            },
            "wallet": CompleteWallet::default(),
        })
    }

    /// Report #58: transferred characters arrived with an empty `quests` table,
    /// so `get_quests` returned `quests: []` and the in-game quest map was
    /// blank. The import body must carry the captured story quests.
    ///
    /// The fixture is a verbatim entry from a real pre-shutdown capture of
    /// `GET /characters/{id}/quests` (capture 462), not an invented shape —
    /// including the `difficultyLevel: -1` that marks a story quest and the
    /// `questId == gldQuestId` identity retail used for them.
    #[test]
    fn import_request_carries_captured_story_quests() {
        let quest_id = "159bc1e7-454c-4e2a-90cf-e200c74b961a";
        let objective_id = "64b2ac8f-9500-4101-b25b-87b41df1b6d7";

        let mut body = body_without_quests();
        body["quests"] = serde_json::json!([{
            "questId": quest_id,
            "version": 2,
            "type": "NORMAL",
            "objectiveStatuses": {
                objective_id: { "status": "Active", "progress": 0.0, "completed": false }
            },
            "difficultyLevel": -1,
            "seed": 485975867u64,
            "gldQuestId": quest_id,
            "completed": false,
        }]);

        let parsed: ImportCharacterRequest =
            serde_json::from_value(body).expect("a captured quests array must deserialize");

        assert_eq!(parsed.quests.len(), 1, "the captured quest must survive");
        let q = &parsed.quests[0];
        assert_eq!(q.quest_id, Uuid::parse_str(quest_id).unwrap());
        assert_eq!(q.quest.gld_quest_id, Uuid::parse_str(quest_id).unwrap());
        assert_eq!(q.quest.seed, serde_json::Number::from(485975867));
        // -1 is what retail sends for story quests; jobs carry a real difficulty.
        assert_eq!(q.quest.difficulty_level, -1);
        assert!(!q.quest.completed);
        assert_eq!(
            q.quest.objective_statuses.len(),
            1,
            "objective progress must not be dropped — it is what the map draws"
        );
    }

    /// Report #59: ONE quest with a value the schema could not read rejected the
    /// entire `import-character` body, and the player could not transfer his
    /// character at all. A quest we cannot parse must cost that quest only.
    ///
    /// The bad entry here is the real shape that caused it — a quest whose
    /// `seed` is not representable — alongside two good ones.
    #[test]
    fn an_unreadable_quest_does_not_reject_the_character() {
        let good = |id: &str| {
            serde_json::json!({
                "questId": id, "version": 2, "type": "NORMAL",
                "objectiveStatuses": {}, "difficultyLevel": -1, "seed": 485975867,
                "gldQuestId": id, "completed": false,
            })
        };
        let mut body = body_without_quests();
        body["quests"] = serde_json::json!([
            good("159bc1e7-454c-4e2a-90cf-e200c74b961a"),
            // unreadable: `seed` is a string, not a number
            {
                "questId": "334e582f-95ba-4263-b381-ac6d91eabe92",
                "version": 2, "type": "NORMAL", "objectiveStatuses": {},
                "difficultyLevel": -1, "seed": "not-a-number",
                "gldQuestId": "334e582f-95ba-4263-b381-ac6d91eabe92",
                "completed": false,
            },
            good("378307c6-0a23-41f8-b721-5282fa0a8a2b"),
        ]);

        let parsed: ImportCharacterRequest = serde_json::from_value(body)
            .expect("one unreadable quest must not reject the whole character");
        assert_eq!(parsed.quests.len(), 2, "the two readable quests survive");
        // And the character itself is intact — the point of the whole change.
        assert_eq!(parsed.user_id.to_string(), "11111111-2222-3333-4444-555555555555");
    }

    /// A quest missing a required field is dropped, not defaulted into
    /// something wrong.
    #[test]
    fn a_quest_missing_a_required_field_is_dropped() {
        let mut body = body_without_quests();
        body["quests"] = serde_json::json!([{ "questId": "159bc1e7-454c-4e2a-90cf-e200c74b961a" }]);
        let parsed: ImportCharacterRequest = serde_json::from_value(body).unwrap();
        assert!(parsed.quests.is_empty());
    }

    /// Leniency applies ONLY to the quest list. A character whose own parts are
    /// unreadable is not importable, and must still be refused.
    #[test]
    fn a_broken_character_part_is_still_refused() {
        let mut body = body_without_quests();
        body["wallet"] = serde_json::json!("not a wallet");
        let r: Result<ImportCharacterRequest, _> = serde_json::from_value(body);
        assert!(r.is_err(), "a malformed wallet must still fail the import");
    }

    /// Older capture-side callers send no `quests` key at all. They must keep
    /// working and simply seed nothing, rather than failing the whole import.
    #[test]
    fn import_request_without_quests_still_deserializes() {
        let parsed: ImportCharacterRequest = serde_json::from_value(body_without_quests())
            .expect("a body predating the quests field must still deserialize");
        assert!(parsed.quests.is_empty());
        assert!(parsed.dungeon_generated_data_list.is_empty());
    }

    /// Captures `log` records so a test can assert that something was
    /// REPORTED, not merely that it returned the right status.
    ///
    /// Only one logger may be installed per process, so this installs once and
    /// hands out an exclusive lease; `start()` returns `None` if some other
    /// logger got there first, which the caller must treat as a skip rather
    /// than a pass. The lease serialises capturing tests against each other so
    /// one cannot drain another's records.
    mod log_capture {
        use std::sync::{Mutex, MutexGuard, Once, OnceLock};

        static RECORDS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
        static LEASE: Mutex<()> = Mutex::new(());
        static INSTALL: Once = Once::new();

        fn records() -> &'static Mutex<Vec<String>> {
            RECORDS.get_or_init(|| Mutex::new(Vec::new()))
        }

        struct Capture;
        impl log::Log for Capture {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, r: &log::Record<'_>) {
                records()
                    .lock()
                    .unwrap()
                    .push(format!("{} {}", r.level(), r.args()));
            }
            fn flush(&self) {}
        }

        pub struct Lease(#[allow(dead_code)] MutexGuard<'static, ()>);

        impl Lease {
            /// Everything logged since the lease began.
            pub fn take(&self) -> Vec<String> {
                std::mem::take(&mut *records().lock().unwrap())
            }
        }

        /// Install the capturing logger and clear the buffer, or `None` if this
        /// process's logger is not ours.
        ///
        /// The control probe matters: `set_boxed_logger` failing is not the only
        /// way to end up capturing nothing, so we prove the pipe works by
        /// pushing a record through it before letting the test rely on it.
        /// Without that, "no records" and "logger not installed" are the same
        /// observation, and the test would report a defect that is not there.
        pub fn start() -> Option<Lease> {
            let guard = LEASE.lock().unwrap_or_else(|e| e.into_inner());
            INSTALL.call_once(|| {
                if log::set_boxed_logger(Box::new(Capture)).is_ok() {
                    log::set_max_level(log::LevelFilter::Trace);
                }
            });
            records().lock().unwrap().clear();
            log::warn!("log-capture control probe");
            let works = records()
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("log-capture control probe"));
            records().lock().unwrap().clear();
            works.then_some(Lease(guard))
        }
    }

    // --- device claims: who may bind, and who may see -----------------------
    //
    // These run the REAL statements — `BIND_DEVICE_SQL` and
    // `RECENT_DEVICES_SQL`, imported from the handlers rather than retyped —
    // against a real Postgres. That is deliberate. The whole guard is a SQL
    // `WHERE` clause and a rows-affected count; a test that re-implemented the
    // rule in Rust would pass just as happily with the guard deleted from the
    // statement the server actually sends, which is the failure mode this
    // repo has shipped before.
    //
    // Each test runs inside its own throwaway schema, in a transaction that is
    // never committed, so they leave nothing behind and can run concurrently.
    //
    // They need a database. CI provides one (see .github/workflows/ci.yml);
    // locally, set TEST_DATABASE_URL. Without it they SKIP rather than fail —
    // and `bind_sql_still_carries_its_guard` below is the backstop that fails
    // loudly if someone strips the guard while the DB tests are skipped.
    mod device_claims {
        use super::super::{
            BIND_DEVICE_NO_SUCH_USER, BIND_DEVICE_SQL, RECENT_DEVICES_SQL, map_bind_device_error,
        };
        use actix_web::{ResponseError, http::StatusCode};
        use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};
        use uuid::Uuid;

        /// The migration's shape, minus the columns these tests do not touch.
        /// Mirrors migrations/2026-06-08-120000-0000_add_device_bindings and
        /// 2026-06-21-000000-0000_device_bindings_wg_ip.
        /// One statement per entry: Postgres refuses to prepare several at once.
        const SCHEMA: [&str; 2] = [
            "CREATE TABLE users (id UUID PRIMARY KEY)",
            "CREATE TABLE device_bindings ( \
                 device_id TEXT PRIMARY KEY, \
                 user_id UUID REFERENCES users(id), \
                 platform TEXT, \
                 last_seen TIMESTAMPTZ NOT NULL DEFAULT now(), \
                 bound_at TIMESTAMPTZ, \
                 source_wg_ip TEXT)",
        ];

        /// A connection in a private schema inside an uncommitted transaction,
        /// or `None` when no test database is configured.
        async fn fixture() -> Option<AsyncPgConnection> {
            let url = std::env::var("TEST_DATABASE_URL").ok()?;
            let mut conn = AsyncPgConnection::establish(&url)
                .await
                .expect("TEST_DATABASE_URL is set but unreachable");
            conn.begin_test_transaction()
                .await
                .expect("could not open a test transaction");
            let schema = format!("t{}", Uuid::new_v4().simple());
            diesel::sql_query(format!("CREATE SCHEMA {schema}"))
                .execute(&mut conn)
                .await
                .unwrap();
            diesel::sql_query(format!("SET LOCAL search_path TO {schema}"))
                .execute(&mut conn)
                .await
                .unwrap();
            for stmt in SCHEMA {
                diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
            }
            Some(conn)
        }

        /// Two players, both known to the arena `users` table.
        async fn two_users(conn: &mut AsyncPgConnection) -> (Uuid, Uuid) {
            let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
            for id in [a, b] {
                diesel::sql_query("INSERT INTO users (id) VALUES ($1)")
                    .bind::<diesel::sql_types::Uuid, _>(id)
                    .execute(conn)
                    .await
                    .unwrap();
            }
            (a, b)
        }

        /// Run the production bind statement. Returns rows affected — zero is
        /// how Postgres reports the guard declining, and what the handler
        /// turns into a 409.
        ///
        /// `own_wg_ips` is the claimant's WireGuard peers as the capture
        /// platform reports them; `None` is the pre-upgrade web build that
        /// sends no list at all.
        async fn bind_with(
            conn: &mut AsyncPgConnection,
            device: &str,
            user: Uuid,
            own_wg_ips: Option<Vec<String>>,
        ) -> usize {
            diesel::sql_query(BIND_DEVICE_SQL)
                .bind::<diesel::sql_types::Text, _>(device.to_string())
                .bind::<diesel::sql_types::Uuid, _>(user)
                .bind::<diesel::sql_types::Nullable<
                    diesel::sql_types::Array<diesel::sql_types::Text>,
                >, _>(own_wg_ips)
                .execute(conn)
                .await
                .expect("the bind statement must be valid SQL")
        }

        /// A claimant with no WireGuard peers at all — the strictest caller,
        /// and the shape most of the pre-existing tests want.
        async fn bind(conn: &mut AsyncPgConnection, device: &str, user: Uuid) -> usize {
            bind_with(conn, device, user, Some(vec![])).await
        }

        /// Seed an unclaimed row the way `anon_log_in` leaves one: nobody owns
        /// it yet, but it remembers the tunnel it was seen on.
        async fn seen_on(conn: &mut AsyncPgConnection, device: &str, wg_ip: Option<&str>) {
            diesel::sql_query(
                "INSERT INTO device_bindings (device_id, user_id, source_wg_ip) \
                 VALUES ($1, NULL, $2)",
            )
            .bind::<diesel::sql_types::Text, _>(device.to_string())
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                wg_ip.map(|s| s.to_string()),
            )
            .execute(conn)
            .await
            .unwrap();
        }

        #[derive(diesel::QueryableByName)]
        struct OwnerRow {
            #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
            user_id: Option<Uuid>,
        }

        async fn owner_of(conn: &mut AsyncPgConnection, device: &str) -> Option<Uuid> {
            let rows: Vec<OwnerRow> =
                diesel::sql_query("SELECT user_id FROM device_bindings WHERE device_id = $1")
                    .bind::<diesel::sql_types::Text, _>(device.to_string())
                    .get_results(conn)
                    .await
                    .unwrap();
            rows.into_iter().next().and_then(|r| r.user_id)
        }

        #[derive(diesel::QueryableByName)]
        struct ListedDevice {
            #[diesel(sql_type = diesel::sql_types::Text)]
            device_id: String,
            #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
            user_id: Option<Uuid>,
            #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
            #[allow(dead_code)]
            platform: Option<String>,
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            #[allow(dead_code)]
            age_seconds: i64,
            #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
            source_wg_ip: Option<String>,
        }

        /// Run the production list statement as `asker` would see it.
        async fn list_for(conn: &mut AsyncPgConnection, asker: Option<Uuid>) -> Vec<ListedDevice> {
            diesel::sql_query(RECENT_DEVICES_SQL)
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(asker)
                .get_results(conn)
                .await
                .expect("the recent-devices statement must be valid SQL")
        }

        /// Skip-with-a-shout, so a skipped run is visible in the CI log rather
        /// than looking like a pass.
        macro_rules! db {
            () => {
                match fixture().await {
                    Some(c) => c,
                    None => {
                        eprintln!("SKIP: TEST_DATABASE_URL unset — device-claim guard NOT verified");
                        return;
                    }
                }
            };
        }

        // -- binding ---------------------------------------------------------

        /// The legitimate flow: launch the game (which leaves an unclaimed row)
        /// then claim it. If this breaks, the feature is gone.
        #[tokio::test]
        async fn claiming_an_unclaimed_device_works() {
            let mut c = db!();
            let (alice, _bob) = two_users(&mut c).await;
            diesel::sql_query("INSERT INTO device_bindings (device_id, user_id) VALUES ('10.9.0.5', NULL)")
                .execute(&mut c)
                .await
                .unwrap();

            assert_eq!(bind(&mut c, "10.9.0.5", alice).await, 1, "an unclaimed device must be claimable");
            assert_eq!(owner_of(&mut c, "10.9.0.5").await, Some(alice));
        }

        /// A device nobody has ever seen: the INSERT half of the upsert.
        #[tokio::test]
        async fn claiming_a_brand_new_device_works() {
            let mut c = db!();
            let (alice, _bob) = two_users(&mut c).await;

            assert_eq!(bind(&mut c, "10.9.0.77", alice).await, 1, "a first-seen device must bind");
            assert_eq!(owner_of(&mut c, "10.9.0.77").await, Some(alice));
        }

        /// Re-claiming your own device is a no-op, not an error. The transfer
        /// flow re-binds every one of a user's peers on each import, so this
        /// path runs constantly.
        #[tokio::test]
        async fn reclaiming_your_own_device_is_idempotent() {
            let mut c = db!();
            let (alice, _bob) = two_users(&mut c).await;
            bind(&mut c, "10.9.0.5", alice).await;

            for attempt in 0..3 {
                assert_eq!(
                    bind(&mut c, "10.9.0.5", alice).await,
                    1,
                    "re-claim #{attempt} of your own device must succeed"
                );
            }
            assert_eq!(owner_of(&mut c, "10.9.0.5").await, Some(alice));
        }

        /// THE BUG. Alice may not take a device bound to Bob — and Bob's row
        /// must be exactly as it was, because `anon_log_in` reads it to decide
        /// whose character Bob's client loads.
        #[tokio::test]
        async fn claiming_another_players_device_is_refused_and_changes_nothing() {
            let mut c = db!();
            let (alice, bob) = two_users(&mut c).await;
            bind(&mut c, "10.9.0.5", bob).await;
            let before = owner_of(&mut c, "10.9.0.5").await;

            assert_eq!(
                bind(&mut c, "10.9.0.5", alice).await,
                0,
                "stealing Bob's device must affect zero rows (the handler's 409)"
            );
            assert_eq!(
                owner_of(&mut c, "10.9.0.5").await,
                before,
                "Bob's binding must survive the attempt untouched"
            );
            assert_eq!(owner_of(&mut c, "10.9.0.5").await, Some(bob));
        }

        /// The refusal must not be a blanket "no". Alice being denied Bob's
        /// device must not stop her claiming a free one — otherwise a passing
        /// suite could mean the endpoint is simply dead.
        #[tokio::test]
        async fn a_refusal_does_not_disable_legitimate_claims() {
            let mut c = db!();
            let (alice, bob) = two_users(&mut c).await;
            bind(&mut c, "10.9.0.5", bob).await;

            assert_eq!(bind(&mut c, "10.9.0.5", alice).await, 0);
            assert_eq!(bind(&mut c, "10.9.0.6", alice).await, 1, "a free device must still bind");
            assert_eq!(owner_of(&mut c, "10.9.0.6").await, Some(alice));
        }

        // -- the unclaimed-but-attributable hole -----------------------------

        /// THE REMAINING BUG, at the layer that has the row.
        ///
        /// `anon_log_in` leaves an unclaimed row the moment Bob launches the
        /// game. Before this change every guard in this file said yes to it,
        /// because none of them looked at whose tunnel it came from.
        #[tokio::test]
        async fn claiming_an_unclaimed_device_seen_on_another_players_tunnel_is_refused() {
            let mut c = db!();
            let (alice, _bob) = two_users(&mut c).await;
            // A stable-hash deviceId — the shape rule 1 in the web layer
            // cannot attribute, which is why the ten production rows got past
            // the first fix.
            seen_on(&mut c, "7d46ac1bf926f22120c2ce82fb834052", Some("10.99.0.37")).await;

            assert_eq!(
                bind_with(
                    &mut c,
                    "7d46ac1bf926f22120c2ce82fb834052",
                    alice,
                    Some(vec!["10.99.0.24".into()]),
                )
                .await,
                0,
                "a device seen on somebody else's peer IP must not be claimable"
            );
            assert_eq!(
                owner_of(&mut c, "7d46ac1bf926f22120c2ce82fb834052").await,
                None,
                "the refusal must leave the row unclaimed, not half-written"
            );
        }

        /// The positive control: the SAME row, claimed by the peer it was seen
        /// on. Without this a `WHERE false` would pass the test above.
        #[tokio::test]
        async fn claiming_an_unclaimed_device_seen_on_your_own_tunnel_works() {
            let mut c = db!();
            let (alice, _bob) = two_users(&mut c).await;
            seen_on(&mut c, "7d46ac1bf926f22120c2ce82fb834052", Some("10.99.0.24")).await;

            assert_eq!(
                bind_with(
                    &mut c,
                    "7d46ac1bf926f22120c2ce82fb834052",
                    alice,
                    Some(vec!["10.99.0.9".into(), "10.99.0.24".into()]),
                )
                .await,
                1,
                "your own tunnel must still claim your own device"
            );
            assert_eq!(
                owner_of(&mut c, "7d46ac1bf926f22120c2ce82fb834052").await,
                Some(alice)
            );
        }

        /// First launch through a path that recorded no tunnel at all. Refusing
        /// this would break the feature for everyone the rigged APK reaches
        /// outside a captured tunnel, so it must stay open.
        #[tokio::test]
        async fn claiming_an_unclaimed_device_with_no_recorded_tunnel_works() {
            let mut c = db!();
            let (alice, _bob) = two_users(&mut c).await;
            seen_on(&mut c, "unattributable-hash", None).await;

            assert_eq!(
                bind_with(&mut c, "unattributable-hash", alice, Some(vec![])).await,
                1,
                "a device with no source_wg_ip is nobody's, so first launch must work"
            );
            assert_eq!(owner_of(&mut c, "unattributable-hash").await, Some(alice));
        }

        /// Re-affirming a binding you already hold is exempt from the tunnel
        /// check. The transfer flow re-binds every peer on each import, and a
        /// row whose `source_wg_ip` drifted must not start failing imports —
        /// nothing is acquired, so there is nobody to protect.
        #[tokio::test]
        async fn reaffirming_your_own_binding_ignores_the_tunnel() {
            let mut c = db!();
            let (alice, _bob) = two_users(&mut c).await;
            seen_on(&mut c, "alices-device", Some("10.99.0.24")).await;
            assert_eq!(
                bind_with(&mut c, "alices-device", alice, Some(vec!["10.99.0.24".into()])).await,
                1
            );

            assert_eq!(
                bind_with(&mut c, "alices-device", alice, Some(vec![])).await,
                1,
                "a re-bind of a row already yours must succeed regardless of $3"
            );
            assert_eq!(owner_of(&mut c, "alices-device").await, Some(alice));
        }

        /// A claimant with no peers at all sends `[]`, and `= ANY('{}')` is
        /// false — so every attributable row is refused. Fail-closed, not
        /// fail-open, which is the difference between an empty list and a
        /// missing one.
        #[tokio::test]
        async fn an_empty_peer_list_claims_nothing_attributable() {
            let mut c = db!();
            let (alice, _bob) = two_users(&mut c).await;
            seen_on(&mut c, "somebodys-device", Some("10.99.0.37")).await;

            assert_eq!(
                bind_with(&mut c, "somebodys-device", alice, Some(vec![])).await,
                0,
                "an empty peer list must not match an attributable row"
            );
        }

        /// A pre-upgrade web build sends no list. It falls back to the old rule
        /// rather than 409-ing every claim in the deploy window. This test
        /// exists so the ramp is a deliberate, visible decision — delete it
        /// when `$3` is tightened to NOT NULL.
        #[tokio::test]
        async fn a_caller_that_sends_no_peer_list_falls_back_to_the_old_rule() {
            let mut c = db!();
            let (alice, bob) = two_users(&mut c).await;
            seen_on(&mut c, "unclaimed-elsewhere", Some("10.99.0.37")).await;
            bind(&mut c, "bobs-device", bob).await;

            assert_eq!(
                bind_with(&mut c, "unclaimed-elsewhere", alice, None).await,
                1,
                "no list = old rule = unclaimed rows still bind"
            );
            assert_eq!(
                bind_with(&mut c, "bobs-device", alice, None).await,
                0,
                "the ramp must not also give away the ownership guard"
            );
            assert_eq!(owner_of(&mut c, "bobs-device").await, Some(bob));
        }

        // -- listing ---------------------------------------------------------

        /// Alice must not be shown Bob's device — not the row, and above all
        /// not Bob's `user_id`, which the web app forwards to a browser.
        #[tokio::test]
        async fn the_list_hides_another_players_device() {
            let mut c = db!();
            let (alice, bob) = two_users(&mut c).await;
            bind(&mut c, "10.9.0.bob", bob).await;

            let seen = list_for(&mut c, Some(alice)).await;
            assert!(
                !seen.iter().any(|d| d.device_id == "10.9.0.bob"),
                "Bob's device must not appear in Alice's claim list"
            );
            assert!(
                !seen.iter().any(|d| d.user_id == Some(bob)),
                "Bob's user_id must never cross the wire to Alice"
            );
        }

        /// The positive control for the test above. Without it, a `WHERE false`
        /// would look like perfect security while breaking the feature.
        #[tokio::test]
        async fn the_list_shows_unclaimed_and_own_devices() {
            let mut c = db!();
            let (alice, bob) = two_users(&mut c).await;
            diesel::sql_query("INSERT INTO device_bindings (device_id, user_id) VALUES ('10.9.0.free', NULL)")
                .execute(&mut c)
                .await
                .unwrap();
            bind(&mut c, "10.9.0.alice", alice).await;
            bind(&mut c, "10.9.0.bob", bob).await;

            let seen = list_for(&mut c, Some(alice)).await;
            assert!(
                seen.iter().any(|d| d.device_id == "10.9.0.free"),
                "an unclaimed device must be listed, or nobody can ever claim anything"
            );
            assert!(
                seen.iter().any(|d| d.device_id == "10.9.0.alice"),
                "Alice's own device must be listed so the UI can mark it hers"
            );
            assert_eq!(seen.len(), 2, "exactly the free one and Alice's own");
        }

        /// A caller that sends no `userId` degrades to unclaimed-only. It must
        /// not fall back to listing everything, which is the original defect.
        #[tokio::test]
        async fn an_anonymous_list_leaks_nothing() {
            let mut c = db!();
            let (alice, bob) = two_users(&mut c).await;
            diesel::sql_query("INSERT INTO device_bindings (device_id, user_id) VALUES ('10.9.0.free', NULL)")
                .execute(&mut c)
                .await
                .unwrap();
            bind(&mut c, "10.9.0.alice", alice).await;
            bind(&mut c, "10.9.0.bob", bob).await;

            let seen = list_for(&mut c, None).await;
            assert!(
                seen.iter().all(|d| d.user_id.is_none()),
                "a userId-less call must return only unclaimed rows, never a bound one"
            );
            assert!(seen.iter().any(|d| d.device_id == "10.9.0.free"));
        }

        /// The list must carry `source_wg_ip`, or the capture platform has no
        /// evidence to judge an unclaimed row with and its own rule degrades to
        /// "nobody has claimed this yet" — the hole.
        // -- reporting the failure -------------------------------------------
        //
        // Nine production 500s on 2026-08-30 came out of this endpoint with no
        // diagnostic at all, because the handler mapped the diesel error with
        // `.map_err(|_| …)`. Postgres's own log had the answer the whole time:
        // `device_bindings_user_id_fkey` — the claim named a user with no row
        // in `users`. Two of the three offending ids had still never existed
        // days later, so this is not a creation race that retrying would fix;
        // it is a caller naming a user that is not there.
        //
        // These two tests pin the two halves of that: the failure must be
        // CLASSIFIED (so the caller gets 404, not 500) and it must be LOGGED
        // (so the next unclassified one is not invisible).

        /// Provoke the REAL production failure: run the real statement against
        /// a real Postgres with a `user_id` that has no `users` row, and take
        /// the error diesel actually produces. Nothing here is synthetic, so
        /// it cannot drift from what the server hits.
        async fn real_fk_violation(conn: &mut AsyncPgConnection) -> diesel::result::Error {
            let ghost = Uuid::new_v4(); // deliberately never inserted into `users`
            diesel::sql_query(BIND_DEVICE_SQL)
                .bind::<diesel::sql_types::Text, _>("10.9.0.201".to_string())
                .bind::<diesel::sql_types::Uuid, _>(ghost)
                .bind::<diesel::sql_types::Nullable<
                    diesel::sql_types::Array<diesel::sql_types::Text>,
                >, _>(Some(Vec::<String>::new()))
                .execute(conn)
                .await
                .expect_err("binding a user with no `users` row must be refused by the FK")
        }

        /// A claim naming a user this server has never heard of is the
        /// CALLER's mistake. It must not come back as a 500.
        #[tokio::test]
        async fn claiming_for_a_user_that_does_not_exist_is_a_404_not_a_500() {
            let mut c = db!();
            let err = real_fk_violation(&mut c).await;

            let mapped = map_bind_device_error(&err);
            assert_eq!(
                mapped.status_code(),
                StatusCode::NOT_FOUND,
                "a claim for a nonexistent arena user must be reported as such, \
                 not as a server fault — got {}",
                mapped.status_code()
            );
            assert_eq!(
                mapped.error_code(),
                BIND_DEVICE_NO_SUCH_USER,
                "the envelope must carry the no-such-user code so the capture \
                 platform can say something useful"
            );
        }

        /// The regression that hid the outage: the diesel error was thrown
        /// away. Whatever the status, the cause must reach the log.
        #[tokio::test]
        async fn a_database_rejection_is_logged_not_swallowed() {
            let mut c = db!();
            let err = real_fk_violation(&mut c).await;

            let Some(captured) = super::log_capture::start() else {
                eprintln!(
                    "SKIP: another logger owns this process — silent-failure regression NOT verified"
                );
                return;
            };

            let _ = map_bind_device_error(&err);

            let lines = captured.take();
            let found = lines
                .iter()
                .find(|l| l.contains("bind-device"))
                .unwrap_or_else(|| {
                    panic!(
                        "mapping a database failure logged NOTHING about bind-device; \
                         that is the defect that hid nine production 500s. Captured: {lines:?}"
                    )
                });
            assert!(
                found.contains("device_bindings_user_id_fkey"),
                "the log line must name the constraint that rejected the write, \
                 or it is not a diagnostic. Got: {found}"
            );
            assert!(
                found.contains("ForeignKeyViolation"),
                "the log line must carry diesel's classification of the SQLSTATE. Got: {found}"
            );
        }

        #[tokio::test]
        async fn the_list_carries_the_tunnel_each_device_was_seen_on() {
            let mut c = db!();
            let (alice, _bob) = two_users(&mut c).await;
            seen_on(&mut c, "seen-somewhere", Some("10.99.0.37")).await;
            seen_on(&mut c, "seen-nowhere", None).await;

            let seen = list_for(&mut c, Some(alice)).await;
            let ip = |id: &str| {
                seen.iter()
                    .find(|d| d.device_id == id)
                    .map(|d| d.source_wg_ip.clone())
            };
            assert_eq!(
                ip("seen-somewhere"),
                Some(Some("10.99.0.37".to_string())),
                "the recorded tunnel must reach the caller"
            );
            assert_eq!(
                ip("seen-nowhere"),
                Some(None),
                "and an unrecorded one must arrive as NULL, not be dropped"
            );
        }

        // -- backstop --------------------------------------------------------

        /// Runs with no database, so the guards cannot be quietly deleted
        /// during a run where the DB tests all skipped. This asserts on the
        /// same constants the handlers execute.
        #[test]
        fn the_statements_still_carry_their_guards() {
            assert!(
                BIND_DEVICE_SQL.contains("device_bindings.user_id IS NULL")
                    && BIND_DEVICE_SQL.contains("device_bindings.user_id = EXCLUDED.user_id"),
                "bind-device lost its ownership guard: {BIND_DEVICE_SQL}"
            );
            assert!(
                BIND_DEVICE_SQL.contains("device_bindings.source_wg_ip = ANY($3::text[])"),
                "bind-device lost its source_wg_ip guard — an unclaimed row seen on \
                 another player's tunnel is claimable again: {BIND_DEVICE_SQL}"
            );
            assert!(
                RECENT_DEVICES_SQL.contains("WHERE user_id IS NULL OR user_id = $1"),
                "recent-devices lost its user filter: {RECENT_DEVICES_SQL}"
            );
            // A whole-token check, not `contains`: `NULL::text AS
            // not_the_source_wg_ip` satisfies a substring match while
            // returning nothing, and that is exactly the kind of green-against-
            // broken-code this suite exists to prevent.
            assert!(
                RECENT_DEVICES_SQL
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(|t| t == "source_wg_ip"),
                "recent-devices stopped returning source_wg_ip, so the capture \
                 platform cannot attribute an unclaimed device: {RECENT_DEVICES_SQL}"
            );
        }
    }
}
