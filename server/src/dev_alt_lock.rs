//! Developer device locks: pin one alt to one device (owner ask, 2026-11-24).
//!
//! "I'd like for developers to be able to lock an alt to a certain device. This
//! way, once logged in, I can play on my ipad, but I can test on pixel 8pro and
//! iphone 7 with other alts."
//!
//! **Why not just two live rows.** A user has ONE live `characters` row
//! (`uq_characters_user_id`) and ~27 call sites resolve the character from
//! `user_id` alone; a `switch-alt` is a SWAP of that row, so two phones cannot
//! play two alts at once (see "WHAT IT IS NOT" in `admin.rs`). Dropping the
//! constraint means re-threading every one of those lookups.
//!
//! **What a lock does instead.** Locking alt X to device D MOVES X into a child
//! account of its own: the parent's newest kept version of X is written onto the
//! child's single live row, and D's `device_bindings` row is pointed at the
//! child. `anon_log_in` resolves a device through its binding first, so D now
//! logs in as the child and plays X, while every other device of the parent
//! keeps playing the parent's live alt — concurrently, because they are two
//! accounts with one row each.
//!
//! **Unlocking** files the child's live row back into the parent's
//! `character_versions` under X (newest version wins, so progress made on the
//! test device is kept) and points D back at the parent. The child account is
//! kept, dormant (`locked_alt_uuid` NULL), and reused by the next lock of D.
//!
//! **While locked** X must not become live on the parent too — a switch, a
//! version restore or an import of X on the parent is refused
//! ([`refuse_if_alt_locked`]). A child account never ranks and holds no name in
//! the registry: its live row is a copy of a parent alt, and listing it would
//! show that alt twice (migrations 2026-11-24-000000/000001).
//!
//! **Who may lock.** These are `X-Import-Token` dev routes like `switch-alt`;
//! the server has no notion of a website role. The website only offers the
//! control to developers and refuses anyone else before calling here.

use std::sync::Arc;

use actix_web::{
    HttpRequest,
    http::StatusCode,
    post,
    web::{self, Json},
};
use blades_lib::user_data::UserAccount;
use diesel::{
    OptionalExtension, insert_into,
    sql_types::{BigInt, Jsonb, Nullable, Text, Uuid as SqlUuid},
};
use diesel_async::{
    AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    admin::{
        IMPORT_SERVICE_ID, check_import_token, snapshot_character, take_parked_alt_rows,
        take_parked_events, take_parked_quests, unpark_alt_rows, unpark_events, unpark_quests,
    },
    json_db::JsonDbWrapper,
    models::UserDBEntry,
    schema::users,
};

/// The alt is live on the parent: switch the parent to another alt first, or
/// the alt would be live twice.
pub(crate) const LOCK_ALT_IS_LIVE: u64 = 140;
/// The alt is locked to a device. Returned by a second lock of it, and by a
/// switch / restore / import of it on the parent.
pub(crate) const LOCK_ALT_LOCKED: u64 = 141;
/// The device already holds a lock; unlock it first.
pub(crate) const LOCK_DEVICE_LOCKED: u64 = 142;
/// The device is not bound to this user.
pub(crate) const LOCK_DEVICE_NOT_YOURS: u64 = 143;
/// Unlock: nothing is locked to that device for this user.
pub(crate) const LOCK_NOT_LOCKED: u64 = 144;
/// The user is itself a lock child; locks do not nest.
pub(crate) const LOCK_ON_CHILD: u64 = 145;

fn refuse(status: StatusCode, code: u64) -> BladeApiError {
    BladeApiError::new(status, IMPORT_SERVICE_ID, code)
}

fn internal(_: diesel::result::Error) -> BladeApiError {
    refuse(StatusCode::INTERNAL_SERVER_ERROR, 8)
}

#[derive(diesel::QueryableByName)]
struct Flag {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    flag: bool,
}

#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
}

/// Whether the lock table exists. The binary can reach production before the
/// hand-applied migration; the guards then have nothing to guard and must not
/// fail the switch, restore or import they sit in.
async fn table_exists(conn: &mut AsyncPgConnection) -> Result<bool, diesel::result::Error> {
    let row: Flag =
        diesel::sql_query("SELECT to_regclass('dev_alt_lock_accounts') IS NOT NULL AS flag")
            .get_result(conn)
            .await?;
    Ok(row.flag)
}

/// Refuse to make `alt` live on `user_id` while it is locked to a device.
///
/// Called by `switch-alt`, a version restore and an import, each after it has
/// locked the parent's live row — the same row a lock takes first, so a lock
/// and a switch of one alt queue rather than interleave.
pub(crate) async fn refuse_if_alt_locked(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
    alt: Option<Uuid>,
) -> Result<(), BladeApiError> {
    let Some(alt) = alt else { return Ok(()) };
    if !table_exists(conn).await.map_err(internal)? {
        return Ok(());
    }
    let row: Flag = diesel::sql_query(
        "SELECT EXISTS (SELECT 1 FROM dev_alt_lock_accounts \
                         WHERE parent_user_id = $1 AND locked_alt_uuid = $2) AS flag",
    )
    .bind::<SqlUuid, _>(user_id)
    .bind::<SqlUuid, _>(alt)
    .get_result(conn)
    .await
    .map_err(internal)?;
    if row.flag {
        return Err(refuse(StatusCode::CONFLICT, LOCK_ALT_LOCKED));
    }
    Ok(())
}

/// Add (`on`) or remove the " (TST)" label on a character's name, in both
/// places the game reads it: `character.name` and `data.customization.Name`.
pub(crate) fn relabel(character: &mut Value, data: &mut Value, on: bool) {
    let suffix = crate::arena::matchmaker::TEST_NAME_SUFFIX;
    let Some(name) = character
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    let base = name
        .strip_suffix(suffix)
        .unwrap_or(&name)
        .trim_end()
        .to_string();
    let want = if on { format!("{base}{suffix}") } else { base };
    if want == name {
        return;
    }
    character["name"] = Value::String(want.clone());
    if let Some(customization) = data.get_mut("customization") {
        crate::character_names::set_customization_name(customization, &want);
    }
}

/// Whether `user_id` is a lock child — ANY lock row, past or present. A
/// dormant child (its alt unlocked) is still a test account: a session it kept
/// past the unlock must not start fighting for real cups.
///
/// `None` when the answer is unknown (table missing, pool or query failure);
/// the matchmaker then falls back to the stored " (TST)" label rather than
/// assuming a normal player ([`crate::arena::matchmaker`] `load_loadout`).
pub(crate) async fn lock_child_status(conn: &mut AsyncPgConnection, user_id: Uuid) -> Option<bool> {
    diesel::sql_query(
        "SELECT EXISTS (SELECT 1 FROM dev_alt_lock_accounts WHERE child_user_id = $1) AS flag",
    )
    .bind::<SqlUuid, _>(user_id)
    .get_result::<Flag>(conn)
    .await
    .map(|f| f.flag)
    .ok()
}

/// [`lock_child_status`], unknown read as "yes": for the paths that must
/// refuse rather than risk moving a child's row (a forced account link).
pub(crate) async fn is_lock_child(conn: &mut AsyncPgConnection, user_id: Uuid) -> bool {
    lock_child_status(conn, user_id).await.unwrap_or(true)
}

/// Whether an account link (forced or not) from `user_id` must be refused: it
/// is a lock child (its row is a parent alt's copy, and "keep played" would
/// move it onto another account). Fails closed when the table exists but cannot be read.
pub(crate) async fn refuses_account_link(conn: &mut AsyncPgConnection, user_id: Uuid) -> bool {
    match lock_child_status(conn, user_id).await {
        Some(child) => child,
        None => table_exists(conn).await.unwrap_or(true),
    }
}

/// Every lock child account, for leaving them out of the AI-opponent pools.
/// Empty when the table is missing.
pub(crate) async fn lock_child_users(
    conn: &mut AsyncPgConnection,
) -> std::collections::HashSet<Uuid> {
    #[derive(diesel::QueryableByName)]
    struct U {
        #[diesel(sql_type = SqlUuid)]
        child_user_id: Uuid,
    }
    diesel::sql_query("SELECT child_user_id FROM dev_alt_lock_accounts")
        .load::<U>(conn)
        .await
        .map(|v| v.into_iter().map(|u| u.child_user_id).collect())
        .unwrap_or_default()
}

/// Every lock child account, in memory, so the matchmaker can mark a fighter
/// (TST) without a database read inside its bounded loadout timeout — a
/// timed-out loadout (the starter) is marked as surely as a loaded one.
/// Filled at startup ([`ensure_table`]) and on every lock; children are never
/// removed (a dormant child stays a test account). `None` until loaded.
static CHILDREN: std::sync::RwLock<Option<std::collections::HashSet<Uuid>>> =
    std::sync::RwLock::new(None);

/// Whether `user_id` is a lock child, from memory. `None` = not loaded.
pub(crate) fn known_child(user_id: Uuid) -> Option<bool> {
    CHILDREN
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|set| set.contains(&user_id)))
}

pub(crate) fn remember_child(user_id: Uuid) {
    if let Ok(mut g) = CHILDREN.write() {
        g.get_or_insert_with(Default::default).insert(user_id);
    }
}

/// The lock table, created at startup if the hand-applied migration has not
/// run yet: every ranking query names it, so a binary that ships first must not
/// break the leaderboards. Same statements as the migration file (idempotent).
pub(crate) const TABLE_DDL: &str =
    include_str!("../../migrations/2026-11-24-000000-0000_dev_alt_locks/up.sql");

pub(crate) async fn ensure_table(pool: &crate::DbPool) {
    let Ok(mut conn) = pool.get().await else {
        log::warn!("[dev-lock] no connection to ensure dev_alt_lock_accounts");
        return;
    };
    if let Err(e) = diesel_async::SimpleAsyncConnection::batch_execute(&mut conn, TABLE_DDL).await {
        log::warn!("[dev-lock] could not ensure dev_alt_lock_accounts: {e}");
    }
    // The name rules too, so the database and this binary never disagree on
    // "(TST)" vs "(Test)" while the hand-applied 000001 is outstanding. A
    // failure (e.g. the 2026-11-16 registry not installed) only logs.
    if let Err(e) = diesel_async::SimpleAsyncConnection::batch_execute(&mut conn, NAMES_DDL).await {
        log::warn!("[dev-lock] could not install the lock-aware name rules: {e}");
    }
    let children = lock_child_users(&mut conn).await;
    log::info!("[dev-lock] {} lock child account(s) known", children.len());
    if let Ok(mut g) = CHILDREN.write() {
        *g = Some(children);
    }
    drop(conn);
    // The rematch rule reads arena_match_results by opponent; build its index
    // without blocking match writes (CONCURRENTLY cannot run in a transaction,
    // so not in the migration batch).
    let pool = pool.clone();
    tokio::spawn(async move {
        let Ok(mut conn) = pool.get().await else {
            return;
        };
        if let Err(e) = diesel_async::SimpleAsyncConnection::batch_execute(
            &mut conn,
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS arena_match_results_by_opponent \
               ON arena_match_results (opponent_character_id, recorded_at DESC)",
        )
        .await
        {
            log::warn!("[dev-lock] could not build arena_match_results_by_opponent: {e}");
        }
    });
}

/// The lock-aware name rules (migration 2026-11-24-000001), also run at startup.
pub(crate) const NAMES_DDL: &str =
    include_str!("../../migrations/2026-11-24-000001-0000_dev_alt_locks_names/up.sql");

/// Whether the (TST) side of a fight earns arena rewards (owner default,
/// 2026-11-24: NO). The alt carries whatever it earns back to the parent on
/// unlock, so paying it would make a device lock a farm. `false` = no gold, XP,
/// gems, chest meter, W/L, sigils or 25-fight count for the test side; the
/// normal player is unaffected by this switch. Flip to `true` to pay it.
pub const TEST_SIDE_EARNS_ARENA_REWARDS: bool = false;

/// How many of a test character's latest fights count as "recent" for the
/// rematch rule.
pub(crate) const RECENT_OPPONENTS: i64 = 3;

#[derive(diesel::QueryableByName)]
struct Pair {
    #[diesel(sql_type = SqlUuid)]
    test_user: Uuid,
    #[diesel(sql_type = SqlUuid)]
    opponent_user: Uuid,
}

/// (test account, opponent account) for the last [`RECENT_OPPONENTS`] fights of
/// every waiting test account in `users`, read from the NORMAL player's match
/// rows (the test side writes none while [`TEST_SIDE_EARNS_ARENA_REWARDS`] is off). The matchmaker prefers anyone else
/// over these (owner, 2026-11-24): a test character should not farm one player.
pub(crate) async fn recent_test_opponents(
    conn: &mut AsyncPgConnection,
    users: &[Uuid],
) -> Result<Vec<(Uuid, Uuid)>, diesel::result::Error> {
    let rows: Vec<Pair> = diesel::sql_query(
        "SELECT DISTINCT dl.child_user_id AS test_user, oc.user_id AS opponent_user \
           FROM dev_alt_lock_accounts dl \
           CROSS JOIN LATERAL ( \
                SELECT r.character_id AS opponent_character_id FROM arena_match_results r \
                 WHERE r.opponent_character_id = dl.child_character_id \
                 ORDER BY r.recorded_at DESC LIMIT $2) last \
           JOIN characters oc ON oc.id = last.opponent_character_id \
          WHERE dl.locked_alt_uuid IS NOT NULL AND dl.child_user_id = ANY($1) \
          LIMIT 64",
    )
    .bind::<diesel::sql_types::Array<SqlUuid>, _>(users)
    .bind::<BigInt, _>(RECENT_OPPONENTS)
    .load(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|p| (p.test_user, p.opponent_user))
        .collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockAltRequest {
    /// The parent account — the one the device is bound to today.
    pub user_id: Uuid,
    /// The alt to pin. Must have a kept version and must not be live.
    pub alt_uuid: Uuid,
    pub device_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockAltResponse {
    pub child_user_id: Uuid,
    /// The child's live row, now holding the alt.
    pub character_id: Uuid,
    pub alt_uuid: Uuid,
    /// The parent's version that was written onto the child.
    pub restored_from: Uuid,
    /// True when an earlier lock of this device left a child to reuse.
    pub reused_child: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlockAltRequest {
    pub user_id: Uuid,
    pub device_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlockAltResponse {
    pub child_user_id: Uuid,
    pub alt_uuid: Uuid,
    /// The parent's new newest version of the alt: the state it reached on the
    /// device.
    pub saved_as: Uuid,
    /// False when the device had meanwhile been bound elsewhere (a sign-in on
    /// it); the binding is then left as it is.
    pub device_returned: bool,
}

/// `POST /…/dev/v1/characters/lock-alt` — pin an alt to a device.
#[post("/blades.bgs.services/api/dev/v1/characters/lock-alt")]
pub async fn lock_alt(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<LockAltRequest>,
) -> Result<Json<LockAltResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let body = body.into_inner();
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, 5))?;
    let out = conn
        .transaction(move |conn| async move { apply_lock_alt(conn, &body).await }.scope_boxed())
        .await?;
    remember_child(out.child_user_id);
    Ok(Json(out))
}

/// `POST /…/dev/v1/characters/unlock-alt` — give the device back to the parent,
/// keeping what the alt did there.
#[post("/blades.bgs.services/api/dev/v1/characters/unlock-alt")]
pub async fn unlock_alt(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<UnlockAltRequest>,
) -> Result<Json<UnlockAltResponse>, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let body = body.into_inner();
    let mut conn = app_state
        .db_pool
        .get()
        .await
        .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, 5))?;
    let out = conn
        .transaction(move |conn| async move { apply_unlock_alt(conn, &body).await }.scope_boxed())
        .await?;
    // End every session of the child: a client still holding one would keep
    // playing (and refreshing) the dormant child instead of logging in again
    // through the device's binding, which now points at the parent.
    end_sessions(&app_state.session_store, &mut conn, out.child_user_id).await;
    Ok(Json(out))
}

/// End every session of `user_id`: the in-memory ones (so the next request is
/// refused) and the persisted rows (so neither a restart nor `auth/refresh`
/// brings one back). Returns how many live sessions were dropped.
pub(crate) async fn end_sessions(
    store: &crate::session::SessionStore,
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
) -> usize {
    let evicted = store
        .evict_other_sessions_for_user(user_id, Uuid::nil())
        .len();
    if let Err(e) = diesel::sql_query("DELETE FROM sessions WHERE user_id = $1")
        .bind::<SqlUuid, _>(user_id)
        .execute(conn)
        .await
    {
        log::warn!("[dev-lock] could not delete persisted sessions of {user_id}: {e}");
    }
    log::info!("[dev-lock] ended {evicted} live session(s) of {user_id}");
    evicted
}

#[derive(diesel::QueryableByName)]
struct LiveAlt {
    #[diesel(sql_type = Nullable<SqlUuid>)]
    source_alt_uuid: Option<Uuid>,
}

#[derive(diesel::QueryableByName)]
struct Holder {
    #[diesel(sql_type = Nullable<SqlUuid>)]
    user_id: Option<Uuid>,
}

#[derive(diesel::QueryableByName)]
struct Version {
    #[diesel(sql_type = SqlUuid)]
    id: Uuid,
    #[diesel(sql_type = Jsonb)]
    character: Value,
    #[diesel(sql_type = Jsonb)]
    data: Value,
    #[diesel(sql_type = Jsonb)]
    inventory: Value,
    #[diesel(sql_type = Jsonb)]
    wallet: Value,
    #[diesel(sql_type = Nullable<Jsonb>)]
    town: Option<Value>,
    #[diesel(sql_type = Jsonb)]
    server_state: Value,
}

#[derive(diesel::QueryableByName)]
struct ChildRow {
    #[diesel(sql_type = SqlUuid)]
    child_user_id: Uuid,
}

/// The work of [`lock_alt`]. Caller supplies the transaction: a device pointed
/// at a child with no character, or a child holding an alt the parent can still
/// switch to, must not be possible.
pub(crate) async fn apply_lock_alt(
    conn: &mut AsyncPgConnection,
    body: &LockAltRequest,
) -> Result<LockAltResponse, BladeApiError> {
    let parent = body.user_id;
    let alt = body.alt_uuid;

    let nested: Flag = diesel::sql_query(
        "SELECT EXISTS (SELECT 1 FROM dev_alt_lock_accounts WHERE child_user_id = $1) AS flag",
    )
    .bind::<SqlUuid, _>(parent)
    .get_result(conn)
    .await
    .map_err(internal)?;
    if nested.flag {
        return Err(refuse(StatusCode::CONFLICT, LOCK_ON_CHILD));
    }

    // The parent's live row first: `switch-alt`, restore and import lock it too,
    // so a switch to this alt racing the lock queues behind it and then meets
    // the guard.
    let live: Option<LiveAlt> =
        diesel::sql_query("SELECT source_alt_uuid FROM characters WHERE user_id = $1 FOR UPDATE")
            .bind::<SqlUuid, _>(parent)
            .get_result(conn)
            .await
            .optional()
            .map_err(internal)?;
    if live.and_then(|l| l.source_alt_uuid) == Some(alt) {
        return Err(refuse(StatusCode::CONFLICT, LOCK_ALT_IS_LIVE));
    }

    let alt_locked: Flag = diesel::sql_query(
        "SELECT EXISTS (SELECT 1 FROM dev_alt_lock_accounts \
                         WHERE parent_user_id = $1 AND locked_alt_uuid = $2) AS flag",
    )
    .bind::<SqlUuid, _>(parent)
    .bind::<SqlUuid, _>(alt)
    .get_result(conn)
    .await
    .map_err(internal)?;
    if alt_locked.flag {
        return Err(refuse(StatusCode::CONFLICT, LOCK_ALT_LOCKED));
    }
    let device_locked: Flag = diesel::sql_query(
        "SELECT EXISTS (SELECT 1 FROM dev_alt_lock_accounts \
                         WHERE device_id = $1 AND locked_alt_uuid IS NOT NULL) AS flag",
    )
    .bind::<Text, _>(&body.device_id)
    .get_result(conn)
    .await
    .map_err(internal)?;
    if device_locked.flag {
        return Err(refuse(StatusCode::CONFLICT, LOCK_DEVICE_LOCKED));
    }

    // Only a device that is already the parent's. Locking moves a binding, and
    // a dev route must not become a way to take somebody else's phone.
    let holder: Option<Holder> =
        diesel::sql_query("SELECT user_id FROM device_bindings WHERE device_id = $1 FOR UPDATE")
            .bind::<Text, _>(&body.device_id)
            .get_result(conn)
            .await
            .optional()
            .map_err(internal)?;
    if holder.and_then(|h| h.user_id) != Some(parent) {
        return Err(refuse(StatusCode::CONFLICT, LOCK_DEVICE_NOT_YOURS));
    }

    let version: Version = diesel::sql_query(
        "SELECT id, \"character\", data, inventory, wallet, town, server_state \
           FROM character_versions \
          WHERE user_id = $1 AND source_alt_uuid = $2 \
          ORDER BY saved_at DESC, id DESC LIMIT 1",
    )
    .bind::<SqlUuid, _>(parent)
    .bind::<SqlUuid, _>(alt)
    .get_result(conn)
    .await
    .optional()
    .map_err(internal)?
    .ok_or_else(|| refuse(StatusCode::NOT_FOUND, 6))?;

    // The child for this (parent, device): an earlier lock's, or a new account.
    // The lock row goes in BEFORE the child's character, so the name registry's
    // commit-time check already sees the child as excluded.
    let existing: Option<ChildRow> = diesel::sql_query(
        "SELECT child_user_id FROM dev_alt_lock_accounts \
          WHERE parent_user_id = $1 AND device_id = $2 FOR UPDATE",
    )
    .bind::<SqlUuid, _>(parent)
    .bind::<Text, _>(&body.device_id)
    .get_result(conn)
    .await
    .optional()
    .map_err(internal)?;
    let reused_child = existing.is_some();
    let child = match existing {
        Some(c) => c.child_user_id,
        None => {
            let child = Uuid::new_v4();
            insert_into(users::table)
                .values(UserDBEntry {
                    id: child,
                    secret_id: Uuid::new_v4(),
                    data: JsonDbWrapper(UserAccount::new_random()),
                })
                .execute(conn)
                .await
                .map_err(internal)?;
            diesel::sql_query(
                "INSERT INTO dev_alt_lock_accounts (child_user_id, parent_user_id, device_id) \
                 VALUES ($1, $2, $3)",
            )
            .bind::<SqlUuid, _>(child)
            .bind::<SqlUuid, _>(parent)
            .bind::<Text, _>(&body.device_id)
            .execute(conn)
            .await
            .map_err(internal)?;
            child
        }
    };

    let Version {
        id: restored_from,
        mut character,
        mut data,
        inventory,
        wallet,
        town,
        mut server_state,
    } = version;
    let parked = take_parked_quests(&mut server_state);
    let parked_events = take_parked_events(&mut server_state);
    let parked_rows = take_parked_alt_rows(&mut server_state);
    // Shown as " (TST)" everywhere its name is read — HUD, profile, social —
    // because the label lives in the stored name, not in one renderer.
    relabel(&mut character, &mut data, true);
    // The parent's version KEEPS its parked guild seat for the length of the
    // lock: it is what the standings, the in-game board and the h2h board read
    // the alt's guild from, and the child (which holds the live seat) is left
    // out of all of them — so the guild's score cannot change. The website
    // rosters skip the child's live row instead (`guild_manage`, `top100_boards`),
    // so the alt is listed once, exactly as before the lock. (Once fork #703
    // lands, parked seats live in `guild_alt_seats (user_id, alt_key)`, which
    // stays the parent's in the same way.)

    let child_row: Option<IdRow> =
        diesel::sql_query("SELECT id FROM characters WHERE user_id = $1 FOR UPDATE")
            .bind::<SqlUuid, _>(child)
            .get_result(conn)
            .await
            .optional()
            .map_err(internal)?;
    let character_id = match child_row {
        // A dormant child: its last occupant was filed back to the parent on
        // unlock, with its quests and alt rows, so it is simply overwritten.
        Some(r) => {
            diesel::sql_query(
                "UPDATE characters SET \"character\" = $2, data = $3, inventory = $4, \
                        wallet = $5, town = $6, server_state = $7, source_alt_uuid = $8 \
                  WHERE id = $1",
            )
            .bind::<SqlUuid, _>(r.id)
            .bind::<Jsonb, _>(&character)
            .bind::<Jsonb, _>(&data)
            .bind::<Jsonb, _>(&inventory)
            .bind::<Jsonb, _>(&wallet)
            .bind::<Nullable<Jsonb>, _>(&town)
            .bind::<Jsonb, _>(&server_state)
            .bind::<SqlUuid, _>(alt)
            .execute(conn)
            .await
            .map_err(internal)?;
            r.id
        }
        None => {
            let id = Uuid::new_v4();
            diesel::sql_query(
                "INSERT INTO characters (id, user_id, \"character\", data, inventory, wallet, \
                                         town, server_state, source_alt_uuid) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind::<SqlUuid, _>(id)
            .bind::<SqlUuid, _>(child)
            .bind::<Jsonb, _>(&character)
            .bind::<Jsonb, _>(&data)
            .bind::<Jsonb, _>(&inventory)
            .bind::<Jsonb, _>(&wallet)
            .bind::<Nullable<Jsonb>, _>(&town)
            .bind::<Jsonb, _>(&server_state)
            .bind::<SqlUuid, _>(alt)
            .execute(conn)
            .await
            .map_err(internal)?;
            id
        }
    };
    // The alt brings what it parked when it was last left: quests, event rows,
    // guild seat and ledgers — exactly what a `switch-alt` to it would restore.
    if let Some(parked) = parked {
        unpark_quests(conn, character_id, parked)
            .await
            .map_err(internal)?;
    }
    unpark_events(conn, character_id, parked_events)
        .await
        .map_err(internal)?;
    unpark_alt_rows(conn, character_id, child, parked_rows)
        .await
        .map_err(internal)?;

    diesel::sql_query(
        "UPDATE dev_alt_lock_accounts \
            SET locked_alt_uuid = $2, child_character_id = $3, \
                locked_at = EXTRACT(EPOCH FROM now())::bigint \
          WHERE child_user_id = $1",
    )
    .bind::<SqlUuid, _>(child)
    .bind::<SqlUuid, _>(alt)
    .bind::<SqlUuid, _>(character_id)
    .execute(conn)
    .await
    .map_err(internal)?;
    diesel::sql_query(
        "UPDATE device_bindings SET user_id = $3, active_alt_uuid = $4 \
          WHERE device_id = $1 AND user_id = $2",
    )
    .bind::<Text, _>(&body.device_id)
    .bind::<SqlUuid, _>(parent)
    .bind::<SqlUuid, _>(child)
    .bind::<SqlUuid, _>(alt)
    .execute(conn)
    .await
    .map_err(internal)?;

    log::info!(
        "[dev-lock] user {parent} locked alt {alt} to device {} (child account {child}, \
         character {character_id}, from version {restored_from}, reused child: {reused_child})",
        crate::log_redact::device_for_log(&body.device_id),
    );
    Ok(LockAltResponse {
        child_user_id: child,
        character_id,
        alt_uuid: alt,
        restored_from,
        reused_child,
    })
}

#[derive(diesel::QueryableByName)]
struct ActiveLock {
    #[diesel(sql_type = SqlUuid)]
    child_user_id: Uuid,
    #[diesel(sql_type = SqlUuid)]
    locked_alt_uuid: Uuid,
}

/// The work of [`unlock_alt`]. Caller supplies the transaction.
pub(crate) async fn apply_unlock_alt(
    conn: &mut AsyncPgConnection,
    body: &UnlockAltRequest,
) -> Result<UnlockAltResponse, BladeApiError> {
    let parent = body.user_id;
    let lock: ActiveLock = diesel::sql_query(
        "SELECT child_user_id, locked_alt_uuid FROM dev_alt_lock_accounts \
          WHERE parent_user_id = $1 AND device_id = $2 AND locked_alt_uuid IS NOT NULL \
          FOR UPDATE",
    )
    .bind::<SqlUuid, _>(parent)
    .bind::<Text, _>(&body.device_id)
    .get_result(conn)
    .await
    .optional()
    .map_err(internal)?
    .ok_or_else(|| refuse(StatusCode::NOT_FOUND, LOCK_NOT_LOCKED))?;
    let (child, alt) = (lock.child_user_id, lock.locked_alt_uuid);

    let character_id = diesel::sql_query("SELECT id FROM characters WHERE user_id = $1 FOR UPDATE")
        .bind::<SqlUuid, _>(child)
        .get_result::<IdRow>(conn)
        .await
        .map_err(internal)?
        .id;

    // Kept under the CHILD first, so its quests, events, guild seat and ledgers
    // are parked by the child's ids — parking by the parent's user id would take
    // the user-keyed rows of the alt the parent is playing. Then the version is
    // handed to the parent, where a `switch-alt` to the alt will find it newest.
    let saved_as = snapshot_character(conn, character_id, child, Some(alt), "dev-unlock", true)
        .await
        .map_err(internal)?;
    // Back on the parent the alt is a player character again: the label goes,
    // and the version is filed under the PARENT's row id — the readers of a
    // switched-out alt (in-game board, guild roster, standings) join versions
    // by `character_id`, and the child's id would make it nobody's.
    #[derive(diesel::QueryableByName)]
    struct Kept {
        #[diesel(sql_type = Jsonb)]
        character: Value,
        #[diesel(sql_type = Jsonb)]
        data: Value,
    }
    let Kept {
        mut character,
        mut data,
    } = diesel::sql_query("SELECT \"character\", data FROM character_versions WHERE id = $1")
        .bind::<SqlUuid, _>(saved_as)
        .get_result(conn)
        .await
        .map_err(internal)?;
    relabel(&mut character, &mut data, false);
    diesel::sql_query(
        "UPDATE character_versions \
            SET user_id = $2, \"character\" = $3, data = $4, \
                name = COALESCE($3->>'name', name), \
                character_id = COALESCE((SELECT c.id FROM characters c WHERE c.user_id = $2), \
                                        character_id) \
          WHERE id = $1",
    )
    .bind::<SqlUuid, _>(saved_as)
    .bind::<SqlUuid, _>(parent)
    .bind::<Jsonb, _>(&character)
    .bind::<Jsonb, _>(&data)
    .execute(conn)
    .await
    .map_err(internal)?;
    // Dormant: the row no longer claims to be the alt.
    diesel::sql_query("UPDATE characters SET source_alt_uuid = NULL WHERE id = $1")
        .bind::<SqlUuid, _>(character_id)
        .execute(conn)
        .await
        .map_err(internal)?;
    diesel::sql_query(
        "UPDATE dev_alt_lock_accounts SET locked_alt_uuid = NULL, locked_at = NULL \
          WHERE child_user_id = $1",
    )
    .bind::<SqlUuid, _>(child)
    .execute(conn)
    .await
    .map_err(internal)?;
    let returned = diesel::sql_query(
        "UPDATE device_bindings SET user_id = $3, active_alt_uuid = NULL \
          WHERE device_id = $1 AND user_id = $2",
    )
    .bind::<Text, _>(&body.device_id)
    .bind::<SqlUuid, _>(child)
    .bind::<SqlUuid, _>(parent)
    .execute(conn)
    .await
    .map_err(internal)?;

    log::info!(
        "[dev-lock] user {parent} unlocked alt {alt} from device {} (child {child}; kept as \
         {saved_as}; device returned: {})",
        crate::log_redact::device_for_log(&body.device_id),
        returned == 1,
    );
    Ok(UnlockAltResponse {
        child_user_id: child,
        alt_uuid: alt,
        saved_as,
        device_returned: returned == 1,
    })
}

/// One device of a user, for the alt picker: the parent's own devices plus the
/// ones currently locked to a child.
#[derive(Serialize, diesel::QueryableByName)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAlt {
    #[diesel(sql_type = Text)]
    pub device_id: String,
    #[diesel(sql_type = Nullable<SqlUuid>)]
    pub active_alt_uuid: Option<Uuid>,
    /// What the client said it is (`android`, `ios`, …), to tell phones apart.
    #[diesel(sql_type = Nullable<Text>)]
    pub platform: Option<String>,
    #[diesel(sql_type = Nullable<BigInt>)]
    pub last_seen_age_seconds: Option<i64>,
    /// The alt locked to this device, if any.
    #[diesel(sql_type = Nullable<SqlUuid>)]
    pub locked_alt_uuid: Option<Uuid>,
}

const DEVICES_SQL: &str = "\
    SELECT d.device_id, d.active_alt_uuid, d.platform, \
           CAST(EXTRACT(epoch FROM (now() - d.last_seen)) AS BIGINT) AS last_seen_age_seconds, \
           l.locked_alt_uuid \
      FROM device_bindings d \
      LEFT JOIN dev_alt_lock_accounts l \
             ON l.device_id = d.device_id AND l.parent_user_id = $1 \
            AND l.locked_alt_uuid IS NOT NULL \
     WHERE d.user_id = $1 OR d.user_id = l.child_user_id \
        OR EXISTS (SELECT 1 FROM dev_alt_lock_accounts x \
                    WHERE x.parent_user_id = $1 AND x.child_user_id = d.user_id \
                      AND x.locked_alt_uuid IS NOT NULL) \
     ORDER BY d.last_seen DESC";

const DEVICES_SQL_NO_LOCKS: &str = "\
    SELECT d.device_id, d.active_alt_uuid, d.platform, \
           CAST(EXTRACT(epoch FROM (now() - d.last_seen)) AS BIGINT) AS last_seen_age_seconds, \
           NULL::uuid AS locked_alt_uuid \
      FROM device_bindings d WHERE d.user_id = $1 ORDER BY d.last_seen DESC";

/// The user's devices with their locks. Falls back to the plain list before the
/// lock migration is applied.
pub(crate) async fn devices_with_locks(
    conn: &mut AsyncPgConnection,
    user_id: Uuid,
) -> Result<Vec<DeviceAlt>, diesel::result::Error> {
    let sql = if table_exists(conn).await? {
        DEVICES_SQL
    } else {
        DEVICES_SQL_NO_LOCKS
    };
    diesel::sql_query(sql)
        .bind::<SqlUuid, _>(user_id)
        .load(conn)
        .await
}

/// The DDL the ranking and switch test fixtures need for the lock exclusion.
#[cfg(test)]
pub(crate) const MIGRATION_FOR_TESTS: &str =
    include_str!("../../migrations/2026-11-24-000000-0000_dev_alt_locks/up.sql");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{SwitchAltRequest, apply_restore, apply_switch_alt};
    use diesel_async::SimpleAsyncConnection;

    /// Every query that lists live characters on a public board leaves lock
    /// children out — a child's row is a copy of a parent alt that the parent
    /// still lists as a kept version, so it would appear twice.
    #[test]
    fn rankings_leave_lock_children_out() {
        let marker = "dev_alt_lock_accounts";
        assert!(crate::arena::leaderboards::RANKED_CTE.contains(marker));
        assert!(crate::arena::season_store::FREEZE_STANDINGS_SQL.contains(marker));
        assert!(crate::arena::ranking::H2H_BOARD_ENTRIES_SQL.contains(marker));
        // A fight WITH a test character is not a monthly or season entry either,
        // for the normal player's side too (owner, 2026-11-24).
        let both_sides = "dl.child_character_id IN (r.character_id, r.opponent_character_id)";
        assert!(crate::arena::monthly_matches::MONTHLY_MATCHES_SQL.contains(both_sides));
        assert!(crate::arena::monthly_matches::SEASON_MATCHES_SQL.contains(both_sides));
        assert!(crate::arena::season_store::FREEZE_STANDINGS_SQL.contains(both_sides));
        assert!(
            include_str!("../../migrations/2026-11-24-000001-0000_dev_alt_locks_names/up.sql")
                .contains("l.child_user_id = a.user_id")
        );
    }

    #[test]
    fn migration_is_idempotent_by_construction() {
        for stmt in MIGRATION_FOR_TESTS.split(';') {
            let s = stmt
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join(" ");
            let s = s.trim();
            if s.is_empty() {
                continue;
            }
            assert!(
                s.contains("IF NOT EXISTS"),
                "every statement must be re-runnable: {s}"
            );
        }
    }

    // ---- database tests (SKIP without TEST_DATABASE_URL) -------------------

    const SCHEMA: [&str; 5] = [
        "CREATE TABLE users (id UUID PRIMARY KEY, secret_id UUID UNIQUE NOT NULL, \
             data JSONB NOT NULL)",
        "CREATE TABLE characters (id UUID PRIMARY KEY, \
             user_id UUID NOT NULL REFERENCES users(id), \
             character JSONB NOT NULL, data JSONB NOT NULL, inventory JSONB NOT NULL, \
             wallet JSONB NOT NULL, town JSONB, \
             server_state JSONB NOT NULL DEFAULT '{}'::jsonb, source_alt_uuid UUID, \
             CONSTRAINT uq_characters_user_id UNIQUE (user_id))",
        "CREATE TABLE quests (id UUID NOT NULL, \
             character_id UUID NOT NULL REFERENCES characters(id), \
             info JSONB NOT NULL, generated_data JSONB NOT NULL, dungeon_state JSONB, \
             initial_state JSONB, PRIMARY KEY (id, character_id))",
        "CREATE TABLE character_versions (id UUID PRIMARY KEY, character_id UUID NOT NULL, \
             user_id UUID NOT NULL, source_alt_uuid UUID, name TEXT NOT NULL DEFAULT '', \
             level INTEGER NOT NULL DEFAULT 0, character JSONB NOT NULL, data JSONB NOT NULL, \
             inventory JSONB NOT NULL, wallet JSONB NOT NULL, town JSONB, \
             server_state JSONB NOT NULL, reason TEXT NOT NULL DEFAULT 'import', \
             saved_at BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint)",
        "CREATE TABLE device_bindings (device_id TEXT PRIMARY KEY, user_id UUID, \
             platform TEXT, last_seen TIMESTAMPTZ NOT NULL DEFAULT now(), \
             bound_at TIMESTAMPTZ, source_wg_ip TEXT, active_alt_uuid UUID)",
    ];

    async fn fixture() -> Option<AsyncPgConnection> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let mut conn = AsyncPgConnection::establish(&url)
            .await
            .expect("TEST_DATABASE_URL is set but unreachable");
        conn.begin_test_transaction().await.unwrap();
        let schema = format!("t{}", Uuid::new_v4().simple());
        conn.batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema}"
        ))
        .await
        .unwrap();
        for stmt in SCHEMA {
            diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
        }
        conn.batch_execute(include_str!(
            "../../migrations/2026-09-04-000000-0000_add_event_quest_tables/up.sql"
        ))
        .await
        .unwrap();
        for ddl in crate::admin::ALT_ROW_TABLE_MIGRATIONS.into_iter().chain([
            include_str!("../../migrations/2026-06-16-000000-0000_add_arena_matches/up.sql"),
            include_str!("../../migrations/2026-07-25-000000-0000_add_arena_match_results/up.sql"),
            include_str!(
                "../../migrations/2026-09-28-000000-0000_add_arena_match_results_h2h_flag/up.sql"
            ),
            include_str!("../../migrations/2026-09-05-000000-0000_add_arena_seasons/up.sql"),
            include_str!("../../migrations/2026-10-10-000000-0000_alts_own_arena_history/up.sql"),
        ]) {
            conn.batch_execute(ddl).await.unwrap();
        }
        crate::testchar::apply_migration_for_tests(&mut conn).await;
        // The real name registry: a lock must get past its commit-time check.
        for ddl in [
            include_str!("../../migrations/2026-11-16-000000-0000_unique_character_names/up.sql"),
            include_str!(
                "../../migrations/2026-11-16-000001-0000_unique_names_holder_check/up.sql"
            ),
            include_str!("../../migrations/2026-11-16-000002-0000_ai_copy_users/up.sql"),
            // Applied twice on purpose: forward-only migrations must be re-runnable.
            MIGRATION_FOR_TESTS,
            include_str!("../../migrations/2026-11-24-000001-0000_dev_alt_locks_names/up.sql"),
            include_str!("../../migrations/2026-11-24-000001-0000_dev_alt_locks_names/up.sql"),
        ] {
            conn.batch_execute(ddl).await.unwrap();
        }
        Some(conn)
    }

    macro_rules! db {
        () => {
            match fixture().await {
                Some(c) => c,
                None => {
                    eprintln!("SKIP: TEST_DATABASE_URL unset — dev device locks NOT verified");
                    return;
                }
            }
        };
    }

    /// The name registry's constraint triggers are DEFERRABLE INITIALLY
    /// DEFERRED, so they only run at commit — which a test transaction never
    /// reaches. Force them now, as a commit would.
    async fn check_deferred(conn: &mut AsyncPgConnection) -> Result<(), diesel::result::Error> {
        conn.batch_execute("SET CONSTRAINTS ALL IMMEDIATE; SET CONSTRAINTS ALL DEFERRED")
            .await
    }

    async fn user(conn: &mut AsyncPgConnection) -> Uuid {
        let id = Uuid::new_v4();
        diesel::sql_query("INSERT INTO users (id, secret_id, data) VALUES ($1, $2, '{}')")
            .bind::<SqlUuid, _>(id)
            .bind::<SqlUuid, _>(Uuid::new_v4())
            .execute(conn)
            .await
            .unwrap();
        id
    }

    async fn live(
        conn: &mut AsyncPgConnection,
        user: Uuid,
        alt: Uuid,
        name: &str,
        level: i64,
    ) -> Uuid {
        let id = Uuid::new_v4();
        diesel::sql_query(
            "INSERT INTO characters (id, user_id, character, data, inventory, wallet, \
                                     server_state, source_alt_uuid) \
             VALUES ($1, $2, $3, '{}', '{}', '{}', '{}', $4)",
        )
        .bind::<SqlUuid, _>(id)
        .bind::<SqlUuid, _>(user)
        .bind::<Jsonb, _>(serde_json::json!({"name": name, "level": level}))
        .bind::<SqlUuid, _>(alt)
        .execute(conn)
        .await
        .unwrap();
        id
    }

    async fn kept(
        conn: &mut AsyncPgConnection,
        character: Uuid,
        user: Uuid,
        alt: Uuid,
        name: &str,
        level: i64,
    ) {
        diesel::sql_query(
            "INSERT INTO character_versions (id, character_id, user_id, source_alt_uuid, name, \
                 level, character, data, inventory, wallet, server_state, saved_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, '{}', '{}', '{}', '{}', 1000)",
        )
        .bind::<SqlUuid, _>(Uuid::new_v4())
        .bind::<SqlUuid, _>(character)
        .bind::<SqlUuid, _>(user)
        .bind::<SqlUuid, _>(alt)
        .bind::<Text, _>(name)
        .bind::<diesel::sql_types::Integer, _>(level as i32)
        .bind::<Jsonb, _>(serde_json::json!({"name": name, "level": level}))
        .execute(conn)
        .await
        .unwrap();
    }

    async fn bind(conn: &mut AsyncPgConnection, device: &str, user: Uuid) {
        diesel::sql_query(
            "INSERT INTO device_bindings (device_id, user_id, platform) VALUES ($1, $2, 'android')",
        )
        .bind::<Text, _>(device)
        .bind::<SqlUuid, _>(user)
        .execute(conn)
        .await
        .unwrap();
    }

    #[derive(diesel::QueryableByName)]
    struct Resolved {
        #[diesel(sql_type = Text)]
        name: String,
        #[diesel(sql_type = BigInt)]
        level: i64,
    }

    /// What a device plays: binding → user → that user's one live row — the
    /// path `anon_log_in` takes.
    async fn plays(conn: &mut AsyncPgConnection, device: &str) -> (String, i64) {
        let r: Resolved = diesel::sql_query(
            "SELECT c.\"character\"->>'name' AS name, \
                    COALESCE((c.\"character\"->>'level')::bigint, 0) AS level \
               FROM device_bindings d JOIN characters c ON c.user_id = d.user_id \
              WHERE d.device_id = $1",
        )
        .bind::<Text, _>(device)
        .get_result(conn)
        .await
        .unwrap();
        (r.name, r.level)
    }

    /// iPad plays alt A; alt B is kept. Returns (parent, live char, alt A, alt B).
    async fn owner(conn: &mut AsyncPgConnection) -> (Uuid, Uuid, Uuid, Uuid) {
        let parent = user(conn).await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let ch = live(conn, parent, a, "Ipadia", 40).await;
        kept(conn, ch, parent, a, "Ipadia", 39).await;
        kept(conn, ch, parent, b, "Pixelle", 12).await;
        bind(conn, "ipad", parent).await;
        bind(conn, "pixel", parent).await;
        (parent, ch, a, b)
    }

    fn lock(user_id: Uuid, alt_uuid: Uuid, device: &str) -> LockAltRequest {
        LockAltRequest {
            user_id,
            alt_uuid,
            device_id: device.to_string(),
        }
    }

    fn code(e: BladeApiError) -> u64 {
        e.error_code()
    }

    #[tokio::test]
    async fn locked_device_and_parent_device_each_play_their_own_alt() {
        let mut c = db!();
        let (parent, _, _, b) = owner(&mut c).await;
        assert_eq!(plays(&mut c, "pixel").await, ("Ipadia".into(), 40));

        let out = apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();
        assert!(!out.reused_child);
        check_deferred(&mut c)
            .await
            .expect("the name registry accepts the lock");

        assert_eq!(plays(&mut c, "ipad").await, ("Ipadia".into(), 40));
        assert_eq!(plays(&mut c, "pixel").await, ("Pixelle (TST)".into(), 12));
    }

    #[tokio::test]
    async fn double_lock_and_busy_device_are_refused() {
        let mut c = db!();
        let (parent, ch, _, b) = owner(&mut c).await;
        let x = Uuid::new_v4();
        kept(&mut c, ch, parent, x, "Iphonia", 5).await;
        bind(&mut c, "iphone7", parent).await;
        apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();

        let again = apply_lock_alt(&mut c, &lock(parent, b, "iphone7"))
            .await
            .unwrap_err();
        assert_eq!(code(again), LOCK_ALT_LOCKED);
        let busy = apply_lock_alt(&mut c, &lock(parent, x, "pixel"))
            .await
            .unwrap_err();
        assert_eq!(code(busy), LOCK_DEVICE_LOCKED);
        // A second alt on a second device is exactly the owner's setup.
        apply_lock_alt(&mut c, &lock(parent, x, "iphone7"))
            .await
            .unwrap();
        assert_eq!(plays(&mut c, "iphone7").await, ("Iphonia (TST)".into(), 5));
        assert_eq!(plays(&mut c, "pixel").await, ("Pixelle (TST)".into(), 12));
        assert_eq!(plays(&mut c, "ipad").await, ("Ipadia".into(), 40));
    }

    #[tokio::test]
    async fn live_alt_foreign_device_and_nested_lock_are_refused() {
        let mut c = db!();
        let (parent, _, a, b) = owner(&mut c).await;
        let live_err = apply_lock_alt(&mut c, &lock(parent, a, "pixel"))
            .await
            .unwrap_err();
        assert_eq!(code(live_err), LOCK_ALT_IS_LIVE);

        let stranger = user(&mut c).await;
        bind(&mut c, "strangers-phone", stranger).await;
        let foreign = apply_lock_alt(&mut c, &lock(parent, b, "strangers-phone"))
            .await
            .unwrap_err();
        assert_eq!(code(foreign), LOCK_DEVICE_NOT_YOURS);

        let out = apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();
        let nested = apply_lock_alt(&mut c, &lock(out.child_user_id, b, "pixel"))
            .await
            .unwrap_err();
        assert_eq!(code(nested), LOCK_ON_CHILD);
    }

    #[tokio::test]
    async fn a_locked_alt_cannot_be_switched_to_or_restored_on_the_parent() {
        let mut c = db!();
        let (parent, _, _, b) = owner(&mut c).await;
        apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();

        let switch = apply_switch_alt(
            &mut c,
            &SwitchAltRequest {
                user_id: parent,
                alt_uuid: b,
                device_id: Some("ipad".into()),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(code(switch), LOCK_ALT_LOCKED);

        let version: IdRow = diesel::sql_query(
            "SELECT id FROM character_versions WHERE user_id = $1 AND source_alt_uuid = $2 LIMIT 1",
        )
        .bind::<SqlUuid, _>(parent)
        .bind::<SqlUuid, _>(b)
        .get_result(&mut c)
        .await
        .unwrap();
        let restore = apply_restore(&mut c, version.id).await.unwrap_err();
        assert_eq!(code(restore), LOCK_ALT_LOCKED);
        assert_eq!(plays(&mut c, "ipad").await, ("Ipadia".into(), 40));
    }

    #[tokio::test]
    async fn unlock_keeps_the_progress_made_on_the_device() {
        let mut c = db!();
        let (parent, _, _, b) = owner(&mut c).await;
        let out = apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();
        // Played on the Pixel: level 12 -> 15.
        diesel::sql_query(
            "UPDATE characters SET \"character\" = '{\"name\":\"Pixelle (TST)\",\"level\":15}' \
              WHERE user_id = $1",
        )
        .bind::<SqlUuid, _>(out.child_user_id)
        .execute(&mut c)
        .await
        .unwrap();

        let un = apply_unlock_alt(
            &mut c,
            &UnlockAltRequest {
                user_id: parent,
                device_id: "pixel".into(),
            },
        )
        .await
        .unwrap();
        assert!(un.device_returned);
        check_deferred(&mut c)
            .await
            .expect("the name registry accepts the unlock");
        assert_eq!(plays(&mut c, "pixel").await, ("Ipadia".into(), 40));

        // Switching the parent to the alt now finds the device's progress.
        apply_switch_alt(
            &mut c,
            &SwitchAltRequest {
                user_id: parent,
                alt_uuid: b,
                device_id: Some("ipad".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(plays(&mut c, "ipad").await, ("Pixelle".into(), 15));

        // And a second unlock has nothing to do.
        let none = apply_unlock_alt(
            &mut c,
            &UnlockAltRequest {
                user_id: parent,
                device_id: "pixel".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(code(none), LOCK_NOT_LOCKED);
    }

    #[tokio::test]
    async fn relocking_a_device_reuses_its_child_account() {
        let mut c = db!();
        let (parent, ch, _, b) = owner(&mut c).await;
        let x = Uuid::new_v4();
        kept(&mut c, ch, parent, x, "Iphonia", 5).await;
        let first = apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();
        apply_unlock_alt(
            &mut c,
            &UnlockAltRequest {
                user_id: parent,
                device_id: "pixel".into(),
            },
        )
        .await
        .unwrap();
        let second = apply_lock_alt(&mut c, &lock(parent, x, "pixel"))
            .await
            .unwrap();
        assert!(second.reused_child);
        assert_eq!(second.child_user_id, first.child_user_id);
        check_deferred(&mut c).await.unwrap();
        assert_eq!(plays(&mut c, "pixel").await, ("Iphonia (TST)".into(), 5));
    }

    #[tokio::test]
    async fn devices_list_includes_locked_devices() {
        let mut c = db!();
        let (parent, _, _, b) = owner(&mut c).await;
        apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();
        let devices = devices_with_locks(&mut c, parent).await.unwrap();
        let pixel = devices
            .iter()
            .find(|d| d.device_id == "pixel")
            .expect("pixel listed");
        assert_eq!(pixel.locked_alt_uuid, Some(b));
        assert_eq!(pixel.platform.as_deref(), Some("android"));
        let ipad = devices
            .iter()
            .find(|d| d.device_id == "ipad")
            .expect("ipad listed");
        assert_eq!(ipad.locked_alt_uuid, None);
        assert_eq!(devices.len(), 2);
    }

    #[test]
    fn parentheses_are_reserved_for_system_labels() {
        use crate::character_names::{has_reserved_brackets, is_exempt};
        for bad in [
            "Ravena (AI)",
            "Bob(",
            "x)",
            "Fullwidth\u{FF08}x\u{FF09}",
            "(TST)",
        ] {
            assert!(has_reserved_brackets(bad), "{bad:?} must be refused");
        }
        for ok in ["Ravena", "O'Neil", "Anna-Lena 2"] {
            assert!(!has_reserved_brackets(ok), "{ok:?} is a fine name");
        }
        assert!(is_exempt("Pixelle (TST)") && is_exempt("Ravena (AI)"));
        assert!(
            !is_exempt("Ravena (Test)"),
            "the old label is no longer a system one"
        );
    }

    #[tokio::test]
    async fn settle_name_refuses_a_bracketed_player_name() {
        let mut c = db!();
        let u = user(&mut c).await;
        let e = crate::character_names::settle_name(&mut c, u, Uuid::new_v4(), "Thorn (Bard)")
            .await
            .unwrap_err();
        assert!(
            crate::character_names::is_reserved_brackets_error(&e),
            "{e:?}"
        );
        // The AI marker is still taken off rather than refused.
        let ok = crate::character_names::settle_name(&mut c, u, Uuid::new_v4(), "Thorn (AI)")
            .await
            .unwrap();
        assert_eq!(ok, "Thorn");
    }

    #[tokio::test]
    async fn lock_labels_the_name_and_unlock_takes_the_label_off() {
        let mut c = db!();
        let (parent, _, _, b) = owner(&mut c).await;
        apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();
        assert_eq!(plays(&mut c, "pixel").await.0, "Pixelle (TST)");
        let un = apply_unlock_alt(
            &mut c,
            &UnlockAltRequest {
                user_id: parent,
                device_id: "pixel".into(),
            },
        )
        .await
        .unwrap();
        #[derive(diesel::QueryableByName)]
        struct Named {
            #[diesel(sql_type = Text)]
            name: String,
            #[diesel(sql_type = Text)]
            json_name: String,
        }
        let v: Named = diesel::sql_query(
            "SELECT name, \"character\"->>'name' AS json_name FROM character_versions WHERE id = $1",
        )
        .bind::<SqlUuid, _>(un.saved_as)
        .get_result(&mut c)
        .await
        .unwrap();
        assert_eq!(
            (v.name.as_str(), v.json_name.as_str()),
            ("Pixelle", "Pixelle")
        );
    }

    async fn fight(
        c: &mut AsyncPgConnection,
        character: Uuid,
        opponent: Uuid,
        win: bool,
        delta: i64,
    ) {
        diesel::sql_query(
            "INSERT INTO arena_match_results (id, character_id, opponent_character_id, win, \
                 rounds_won, rounds_lost, trophy_delta, recorded_at) \
             VALUES ($1, $2, $3, $4, 2, 0, $5, now())",
        )
        .bind::<SqlUuid, _>(Uuid::new_v4())
        .bind::<SqlUuid, _>(character)
        .bind::<SqlUuid, _>(opponent)
        .bind::<diesel::sql_types::Bool, _>(win)
        .bind::<BigInt, _>(delta)
        .execute(c)
        .await
        .unwrap();
    }

    /// The normal player's side of a fight with a test character never reaches
    /// the season standings (Top 100, awards, guild score) — not even as a match
    /// played — and the test side never does either.
    #[tokio::test]
    async fn fights_with_a_test_character_stay_out_of_the_standings() {
        let mut c = db!();
        let (parent, _, _, b) = owner(&mut c).await;
        let locked = apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();
        let human = user(&mut c).await;
        let human_char = live(&mut c, human, Uuid::new_v4(), "Normal", 30).await;
        let third = user(&mut c).await;
        let third_char = live(&mut c, third, Uuid::new_v4(), "Third", 30).await;
        fight(&mut c, human_char, locked.character_id, true, 0).await;
        fight(&mut c, locked.character_id, human_char, false, 0).await;
        fight(&mut c, human_char, third_char, true, 30).await; // the control

        #[derive(diesel::QueryableByName)]
        struct Standing {
            #[diesel(sql_type = SqlUuid)]
            character_id: Uuid,
            #[diesel(sql_type = BigInt)]
            matches: i64,
        }
        let rows: Vec<Standing> =
            diesel::sql_query(crate::arena::season_store::FREEZE_STANDINGS_SQL)
                .bind::<BigInt, _>(0i64)
                .bind::<BigInt, _>(i64::from(i32::MAX))
                .load(&mut c)
                .await
                .unwrap();
        assert!(rows.iter().all(|r| r.character_id != locked.character_id));
        let h = rows
            .iter()
            .find(|r| r.character_id == human_char)
            .expect("human ranks");
        assert_eq!(h.matches, 1, "only the fight against a real player counts");
    }

    #[tokio::test]
    async fn recent_opponents_of_a_test_character_are_found() {
        let mut c = db!();
        let (parent, _, _, b) = owner(&mut c).await;
        let locked = apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();
        let human = user(&mut c).await;
        let human_char = live(&mut c, human, Uuid::new_v4(), "Normal", 30).await;
        // The normal player's row: the test side writes none.
        fight(&mut c, human_char, locked.character_id, false, 0).await;
        let pairs = recent_test_opponents(&mut c, &[locked.child_user_id, human])
            .await
            .unwrap();
        assert_eq!(pairs, vec![(locked.child_user_id, human)]);
        assert!(
            recent_test_opponents(&mut c, &[human])
                .await
                .unwrap()
                .is_empty()
        );
        assert!(is_lock_child(&mut c, locked.child_user_id).await);
        assert!(!is_lock_child(&mut c, parent).await);
    }

    // ---- review fixes (2026-11-24) ---------------------------------------

    async fn unlock(c: &mut AsyncPgConnection, parent: Uuid) -> UnlockAltResponse {
        apply_unlock_alt(
            c,
            &UnlockAltRequest {
                user_id: parent,
                device_id: "pixel".into(),
            },
        )
        .await
        .unwrap()
    }

    /// A dormant child is still a test account, and unlock ends its sessions
    /// in memory and in the database.
    #[tokio::test]
    async fn an_unlocked_child_stays_a_test_account_and_loses_its_sessions() {
        let mut c = db!();
        let (parent, _, _, b) = owner(&mut c).await;
        let locked = apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();
        unlock(&mut c, parent).await;
        assert_eq!(
            lock_child_status(&mut c, locked.child_user_id).await,
            Some(true)
        );
        assert_eq!(lock_child_status(&mut c, parent).await, Some(false));

        c.batch_execute(
            "CREATE TABLE sessions (session_id UUID PRIMARY KEY, user_id UUID NOT NULL, \
                 secret_user_id UUID, extra_secret TEXT, expires_at TIMESTAMPTZ)",
        )
        .await
        .unwrap();
        diesel::sql_query("INSERT INTO sessions (session_id, user_id) VALUES ($1, $2), ($3, $4)")
            .bind::<SqlUuid, _>(Uuid::new_v4())
            .bind::<SqlUuid, _>(locked.child_user_id)
            .bind::<SqlUuid, _>(Uuid::new_v4())
            .bind::<SqlUuid, _>(parent)
            .execute(&mut c)
            .await
            .unwrap();
        let store = crate::session::SessionStore::new(std::time::Duration::from_secs(3600));
        let mk = |user: Uuid| {
            std::sync::Arc::new(crate::session::Session::new(
                user,
                Uuid::new_v4(),
                std::time::Duration::from_secs(3600),
            ))
        };
        let (child_session, parent_session) = (Uuid::new_v4(), Uuid::new_v4());
        store.insert_existing(child_session, mk(locked.child_user_id));
        store.insert_existing(parent_session, mk(parent));
        assert_eq!(end_sessions(&store, &mut c, locked.child_user_id).await, 1);
        assert!(
            !store.contains(child_session),
            "the child's live session is gone"
        );
        assert!(store.contains(parent_session), "the parent's is untouched");
        #[derive(diesel::QueryableByName)]
        struct N {
            #[diesel(sql_type = BigInt)]
            n: i64,
        }
        let n: N = diesel::sql_query("SELECT COUNT(*) AS n FROM sessions WHERE user_id = $1")
            .bind::<SqlUuid, _>(locked.child_user_id)
            .get_result(&mut c)
            .await
            .unwrap();
        assert_eq!(n.n, 0, "persisted child sessions are gone");
        assert!(
            refuses_account_link(&mut c, locked.child_user_id).await,
            "dormant child"
        );
        assert!(!refuses_account_link(&mut c, parent).await);
    }

    #[tokio::test]
    async fn a_forced_link_is_refused_for_a_locked_child() {
        let mut c = db!();
        let (parent, _, _, b) = owner(&mut c).await;
        let locked = apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();
        assert!(refuses_account_link(&mut c, locked.child_user_id).await);
        assert!(!refuses_account_link(&mut c, parent).await);
        assert!(
            lock_child_users(&mut c)
                .await
                .contains(&locked.child_user_id)
        );
    }

    /// The unlocked version is filed under the PARENT's row, where every reader
    /// of a switched-out alt joins it.
    #[tokio::test]
    async fn unlock_files_the_version_under_the_parents_row() {
        let mut c = db!();
        let (parent, parent_char, _, b) = owner(&mut c).await;
        apply_lock_alt(&mut c, &lock(parent, b, "pixel"))
            .await
            .unwrap();
        let un = unlock(&mut c, parent).await;
        let row: IdRow =
            diesel::sql_query("SELECT character_id AS id FROM character_versions WHERE id = $1")
                .bind::<SqlUuid, _>(un.saved_as)
                .get_result(&mut c)
                .await
                .unwrap();
        assert_eq!(row.id, parent_char);
    }

    /// Owner rule: a lock changes no guild score. The alt's season standing and
    /// the guild it counts for are identical before, during and after a lock,
    /// and the website roster lists it once throughout.
    #[tokio::test]
    async fn a_lock_changes_no_guild_score_and_the_roster_lists_the_alt_once() {
        let mut c = db!();
        let (parent, parent_char, _, _) = owner(&mut c).await;
        let x = Uuid::new_v4();
        c.batch_execute("INSERT INTO guilds (id, name, tag_id) VALUES ('g1', 'Guild', '1234')")
            .await
            .unwrap();
        diesel::sql_query(
            "INSERT INTO character_versions (id, character_id, user_id, source_alt_uuid, name, \
                 level, character, data, inventory, wallet, server_state, saved_at) \
             VALUES ($1, $2, $3, $4, 'Seated', 9, '{\"name\":\"Seated\",\"level\":9}', '{}', '{}', '{}', \
                     '{\"parkedGuildMembers\":[{\"guild_id\":\"g1\",\"rank\":\"MEMBER\",\"join_date\":1}]}', 2000)",
        )
        .bind::<SqlUuid, _>(Uuid::new_v4())
        .bind::<SqlUuid, _>(parent_char)
        .bind::<SqlUuid, _>(parent)
        .bind::<SqlUuid, _>(x)
        .execute(&mut c)
        .await
        .unwrap();
        // X's season: one win worth 30 cups, fought on the parent's row.
        diesel::sql_query(
            "INSERT INTO arena_match_results (id, character_id, win, rounds_won, rounds_lost, \
                 trophy_delta, recorded_at, source_alt_uuid) VALUES ($1, $2, true, 2, 0, 30, now(), $3)",
        )
        .bind::<SqlUuid, _>(Uuid::new_v4())
        .bind::<SqlUuid, _>(parent_char)
        .bind::<SqlUuid, _>(x)
        .execute(&mut c)
        .await
        .unwrap();

        #[derive(diesel::QueryableByName, Debug, PartialEq)]
        struct Standing {
            #[diesel(sql_type = Nullable<SqlUuid>)]
            source_alt_uuid: Option<Uuid>,
            #[diesel(sql_type = BigInt)]
            trophies: i64,
            #[diesel(sql_type = Nullable<Text>)]
            guild_id: Option<String>,
        }
        async fn standing(c: &mut AsyncPgConnection, x: Uuid) -> Vec<Standing> {
            let rows: Vec<Standing> =
                diesel::sql_query(crate::arena::season_store::FREEZE_STANDINGS_SQL)
                    .bind::<BigInt, _>(0i64)
                    .bind::<BigInt, _>(i64::from(i32::MAX))
                    .load(c)
                    .await
                    .unwrap();
            rows.into_iter()
                .filter(|r| r.source_alt_uuid == Some(x))
                .collect()
        }
        async fn roster_entries(c: &mut AsyncPgConnection) -> usize {
            crate::guild_manage::live_seats_of_guild(c, "g1")
                .await
                .unwrap()
                .len()
                + crate::guild_manage::parked_seats_of_guild(c, "g1")
                    .await
                    .unwrap()
                    .len()
        }

        let before = standing(&mut c, x).await;
        assert_eq!(
            before,
            vec![Standing {
                source_alt_uuid: Some(x),
                trophies: 30,
                guild_id: Some("g1".into())
            }]
        );
        assert_eq!(roster_entries(&mut c).await, 1);

        let locked = apply_lock_alt(&mut c, &lock(parent, x, "pixel"))
            .await
            .unwrap();
        assert_eq!(standing(&mut c, x).await, before, "during the lock");
        assert_eq!(
            roster_entries(&mut c).await,
            1,
            "listed once during the lock"
        );
        // The seat is physically the child's while it plays.
        let live: IdRow =
            diesel::sql_query("SELECT character_id AS id FROM guild_members WHERE guild_id = 'g1'")
                .get_result(&mut c)
                .await
                .unwrap();
        assert_eq!(live.id, locked.character_id);

        unlock(&mut c, parent).await;
        assert_eq!(standing(&mut c, x).await, before, "after the unlock");
        assert_eq!(roster_entries(&mut c).await, 1);
    }

    #[test]
    fn the_website_guild_roster_leaves_the_child_out() {
        assert!(
            crate::arena::top100_boards::GUILD_ROSTER_SQL.contains("dl.child_character_id = c.id")
        );
    }

    /// Both link endpoints refuse a lock child (D).
    #[test]
    fn both_account_link_endpoints_refuse_a_lock_child() {
        let src = include_str!("authentification.rs");
        let link = src.find("async fn bnet_link(").unwrap();
        let force = src.find("async fn bnet_link_force").unwrap();
        let after = src[force..].find("\n}\n").unwrap() + force;
        assert!(src[link..force].contains("refuses_account_link(&mut conn, caller)"));
        assert!(src[force..after].contains("refuses_account_link(&mut conn, source_user_id)"));
    }

    #[test]
    fn the_child_cache_marks_without_a_database() {
        let child = Uuid::new_v4();
        remember_child(child);
        assert_eq!(known_child(child), Some(true));
        assert_eq!(known_child(Uuid::new_v4()), Some(false));
    }

    /// The normal player's wins against a (TST) character are not counted on
    /// the in-game board, and the cups recalc never replays such a fight.
    #[test]
    fn wins_and_recalcs_leave_test_fights_out() {
        let both_sides = "dl.child_character_id IN (r.character_id, r.opponent_character_id)";
        let season_wins = crate::arena::leaderboards::RANKED_CTE
            .split("season_wins AS")
            .nth(1)
            .and_then(|s| s.split("ranked AS").next())
            .expect("season_wins CTE");
        assert!(season_wins.contains(both_sides));
        assert!(crate::arena::cups_rule::SEASON_ROWS_SQL.contains(both_sides));
    }
}
