use actix_web::{FromRequest, get, http::StatusCode, web};
use log::error;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex, mpsc::UnboundedSender},
    time::Instant,
};
use uuid::Uuid;

use crate::{BladeApiError, DbPool, ServerGlobal, arena::MatchmakingMessage};

pub struct Session {
    pub user_id: Uuid,
    pub secret_user_id: Uuid,
    pub extra_secret: Uuid, // a UUIDv4 just for added randomness
    pub expire_unix_timestamp: u64,
    // incremented each (connected) request by the middleware
    pub request_count: AtomicU64,
    pub matchmaking_ws: Mutex<Option<UnboundedSender<MatchmakingMessage>>>,
}

impl Session {
    pub fn new(user_id: Uuid, secret_user_id: Uuid, ttl: Duration) -> Self {
        Self {
            user_id,
            secret_user_id,
            expire_unix_timestamp: match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(duration) => (duration + ttl - ttl / 10).as_secs(),
                Err(e) => {
                    error!(
                        "Oh no! In Session, it seems we are before the unix timestamp! Defaulting to ttl to 0. Error is {:?}",
                        e
                    );
                    (ttl - ttl / 10).as_secs()
                }
            },
            extra_secret: Uuid::new_v4(),
            request_count: AtomicU64::new(1),
            matchmaking_ws: Mutex::new(None),
        }
    }

    pub fn generate_token(&self, session_id: &Uuid) -> String {
        format!("{}|{}", session_id, self.extra_secret)
    }

    /// Last REST request index accepted for this session. The middleware increments
    /// before handling a request, matching the `/public/sync` response semantics.
    pub fn current_request_index(&self) -> u64 {
        self.request_count
            .load(Ordering::Relaxed)
            .saturating_sub(1)
    }

    /// Claim the matchmaking-feed slot for a freshly opened rms WebSocket.
    ///
    /// Last writer wins: the client reconnects this socket constantly, and the
    /// newest one is always the live one.
    pub async fn set_matchmaking_ws(&self, tx: UnboundedSender<MatchmakingMessage>) {
        *self.matchmaking_ws.lock().await = Some(tx);
    }

    /// Release the slot on socket teardown, but ONLY if it still holds `tx`.
    /// Returns whether it was cleared.
    ///
    /// A blind `= None` here is a real bug, not a tidiness question. A reconnect
    /// registers the new sender BEFORE the old socket notices it is dead, so the
    /// dying socket's teardown would wipe the live socket's sender. Since
    /// `create_match` refuses to queue (409-4-1) whenever this slot is empty, that
    /// left matchmaking permanently broken while the WebSocket kept exchanging
    /// ping/pong normally — invisible until you correlate the 101 upgrades against
    /// the 409s.
    pub async fn clear_matchmaking_ws_if_owner(
        &self,
        tx: &UnboundedSender<MatchmakingMessage>,
    ) -> bool {
        let mut slot = self.matchmaking_ws.lock().await;
        let is_owner = slot.as_ref().is_some_and(|cur| cur.same_channel(tx));
        if is_owner {
            *slot = None;
        }
        is_owner
    }

    /// Whether a matchmaking feed is currently registered (what `create_match`
    /// gates on).
    pub async fn has_matchmaking_ws(&self) -> bool {
        self.matchmaking_ws.lock().await.is_some()
    }
}

#[cfg(test)]
mod matchmaking_slot_tests {
    use super::*;
    use crate::arena::MatchmakingMessage;
    use std::time::Duration as StdDuration;
    use tokio::sync::mpsc::unbounded_channel;

    fn session() -> Session {
        Session::new(Uuid::new_v4(), Uuid::new_v4(), StdDuration::from_secs(3600))
    }

    fn chan() -> UnboundedSender<MatchmakingMessage> {
        unbounded_channel::<MatchmakingMessage>().0
    }

    /// THE REGRESSION. Reproduces the production sequence of 2026-07-30: socket A
    /// opens, a match is queued fine, the client reconnects as socket B, then A's
    /// teardown fires. Before the fix that teardown emptied the slot and every
    /// later matches/create answered 409-4-1.
    #[tokio::test]
    async fn reconnect_then_old_socket_teardown_keeps_the_live_feed() {
        let s = session();
        let a = chan();
        let b = chan();

        s.set_matchmaking_ws(a.clone()).await;
        assert!(s.has_matchmaking_ws().await, "socket A should be queueable");

        // Client reconnects; B takes over the slot.
        s.set_matchmaking_ws(b.clone()).await;

        // A finally notices it is dead and tears down — it must NOT clear B.
        let cleared = s.clear_matchmaking_ws_if_owner(&a).await;
        assert!(!cleared, "A must not clear a slot it no longer owns");
        assert!(
            s.has_matchmaking_ws().await,
            "the live socket B must still be able to queue a match (409-4-1 bug)"
        );
    }

    #[tokio::test]
    async fn the_owning_socket_does_clear_its_own_slot() {
        let s = session();
        let a = chan();
        s.set_matchmaking_ws(a.clone()).await;

        assert!(s.clear_matchmaking_ws_if_owner(&a).await);
        assert!(
            !s.has_matchmaking_ws().await,
            "a genuine disconnect must leave no feed, so create_match correctly refuses"
        );
    }

    #[tokio::test]
    async fn clearing_an_empty_slot_is_a_no_op() {
        let s = session();
        assert!(!s.clear_matchmaking_ws_if_owner(&chan()).await);
        assert!(!s.has_matchmaking_ws().await);
    }

    /// Clones of one socket's sender share a channel, so either must be able to
    /// release it — `same_channel` compares the channel, not the handle.
    #[tokio::test]
    async fn a_clone_of_the_owner_still_counts_as_the_owner() {
        let s = session();
        let a = chan();
        s.set_matchmaking_ws(a.clone()).await;
        assert!(s.clear_matchmaking_ws_if_owner(&a.clone()).await);
    }

    /// Out-of-order teardown: several stale sockets closing in any order must
    /// never disturb the newest registration.
    #[tokio::test]
    async fn many_stale_teardowns_cannot_starve_the_newest_socket() {
        let s = session();
        let stale: Vec<_> = (0..5).map(|_| chan()).collect();
        for tx in &stale {
            s.set_matchmaking_ws(tx.clone()).await;
        }
        let live = chan();
        s.set_matchmaking_ws(live.clone()).await;

        for tx in stale.iter().rev() {
            assert!(!s.clear_matchmaking_ws_if_owner(tx).await);
        }
        assert!(s.has_matchmaking_ws().await, "newest socket must survive");
        assert!(s.clear_matchmaking_ws_if_owner(&live).await);
    }
}

//TODO: FromRequest for this SessionLookupUp
pub struct SessionLookedUp {
    #[allow(unused)]
    pub session_id: Uuid,
    pub session: Arc<Session>,
}

// Read the session from the Authorization header
pub struct SessionLookedUpMaybe(Option<SessionLookedUp>);

impl SessionLookedUpMaybe {
    pub fn get_session_or_error(&self) -> Result<&SessionLookedUp, BladeApiError> {
        self.0
            .as_ref()
            .ok_or_else(|| BladeApiError::new(StatusCode::UNAUTHORIZED, 3, 43))
    }
}

impl FromRequest for SessionLookedUpMaybe {
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    //TODO: use BladeApiError instead
    fn from_request(
        req: &actix_web::HttpRequest,
        _payload: &mut actix_web::dev::Payload,
    ) -> Self::Future {
        // Clone the cheap handles out BEFORE the async move (can't hold &req across .await).
        let authorization = req.headers().get("Authorization").cloned();
        let global = req
            .app_data::<web::Data<Arc<ServerGlobal>>>()
            .expect("server global not in app_data (for extracting a Session)")
            .clone();

        Box::pin(async move {
            let Some(authorization) = authorization else {
                return Ok(SessionLookedUpMaybe(None));
            };
            let authorization = match authorization.to_str() {
                Ok(token) => token,
                Err(_) => {
                    return Err(actix_web::error::ErrorBadRequest(
                        "Authorization header can’t be parsed as str",
                    ));
                }
            };

            // A Blades session token is `…=<session_id>|<extra_secret>`. A header
            // that isn't that shape — notably `Authorization: Bearer <token>` used
            // by our out-of-band tooling routes (admin import, arena debug-inject)
            // — is simply "no session": let it through as `None` so the route's own
            // token check runs, instead of 400-ing every Bearer request in the
            // global session middleware (which pre-empted those handlers entirely).
            let token = match authorization.split('=').nth(1) {
                Some(token) => token,
                None => return Ok(SessionLookedUpMaybe(None)),
            };

            let mut token_splitted = token.split('|');
            let (session_id, extra_secret) = if let Some(session_id) = token_splitted.next()
                && let Some(extra_secret) = token_splitted.next()
            {
                let session_id = match Uuid::parse_str(session_id) {
                    Ok(v) => v,
                    Err(_err) => {
                        return Err(actix_web::error::ErrorBadRequest(
                            "can’t parse session id part of the token",
                        ));
                    }
                };
                let extra_secret = match Uuid::parse_str(extra_secret) {
                    Ok(v) => v,
                    Err(_err) => {
                        return Err(actix_web::error::ErrorBadRequest(
                            "can’t parse extra secret part of the token",
                        ));
                    }
                };
                (session_id, extra_secret)
            } else {
                return Err(actix_web::error::ErrorBadRequest(
                    "Invalid token format (no |)",
                ));
            };

            // In-memory first; on a cold miss (e.g. just after a restart emptied the map)
            // fall back to the persisted `sessions` table and repopulate, so an
            // arena-server rebuild no longer logs everyone out.
            let session = match global.session_store.get(session_id) {
                Some(v) => v,
                None => match load_persisted_session(&global.db_pool, session_id).await {
                    Some(s) => global
                        .session_store
                        .insert_existing(session_id, Arc::new(s)),
                    None => return Ok(SessionLookedUpMaybe(None)),
                },
            };
            if session.extra_secret == extra_secret {
                Ok(SessionLookedUpMaybe(Some(SessionLookedUp {
                    session_id,
                    session,
                })))
            } else {
                Err(actix_web::error::ErrorUnauthorized(
                    "Invalid token (extra secret mismatch)",
                ))
            }
        })
    }
}

pub struct SessionStore {
    //TODO: eventually migrate to a parallel ordered map. A mutex per request seems pretty bad for performance.
    map: std::sync::Mutex<BTreeMap<Uuid, Arc<Session>>>,
    /// TTL should be at least 1h30min, as that is the grace period used by session for its ttl returned to the client.
    pub ttl: Duration,
    time_base: Instant,
}

impl SessionStore {
    pub fn new(ttl: Duration) -> Self {
        Self {
            map: std::sync::Mutex::new(BTreeMap::default()),
            ttl,
            time_base: Instant::now(),
        }
    }

    /// While extremly unlikely, it might generate an already existing key. Another one should be requested in such case.
    /// The UUID encode time since self.time_base in its first 64 bytes (BE-encoded for sorting)
    fn get_uuid_for_instant(&self, future_instant: &Instant) -> Uuid {
        let t = future_instant
            .duration_since(self.time_base)
            .as_secs()
            .to_be_bytes();
        let r: [u8; 8] = rand::random();
        let bytes = [
            t[0], t[1], t[2], t[3], t[4], t[5], t[6], t[7], r[0], r[1], r[2], r[3], r[4], r[5],
            r[6], r[7],
        ];
        Uuid::new_v8(bytes)
    }

    #[allow(unused)]
    pub fn extract_creation_instant(&self, uuid: Uuid) -> Option<Instant> {
        let bytes = uuid.as_bytes();
        let ts_bytes: [u8; 8] = bytes[0..8].try_into().ok()?;
        let secs = u64::from_be_bytes(ts_bytes);
        Some(self.time_base + Duration::from_secs(secs))
    }

    pub fn get(&self, session_id: Uuid) -> Option<Arc<Session>> {
        self.map.lock().unwrap().get(&session_id).cloned()
    }

    /// Insert a session under a KNOWN id (cold-path repopulation from the DB after a
    /// restart — see load_persisted_session). Idempotent: if a concurrent request
    /// already repopulated it, keep that Arc so request_count/matchmaking_ws stay coherent.
    pub fn insert_existing(&self, session_id: Uuid, session: Arc<Session>) -> Arc<Session> {
        self.map
            .lock()
            .unwrap()
            .entry(session_id)
            .or_insert(session)
            .clone()
    }

    /// Drop every OTHER live session belonging to this user.
    ///
    /// One device owns the account at a time. Owner's rule, 2026-09-23: "one
    /// device owns the sessions, and they cannot both be active … opening
    /// device 1 automatically loads from the beginning, but it does not need a
    /// login, or character transfer."
    ///
    /// That is exactly what dropping the session achieves and why nothing
    /// heavier is needed. The old device's next request presents a token this
    /// store no longer knows, gets a 401, and the client re-authenticates by
    /// itself — anonymous login resolves the claimed device straight back to
    /// the same account, so the player sees a reload, not a login screen and
    /// not a lost character.
    ///
    /// It also closes the self-match hole from the other side: two live
    /// sessions for one account are what let a player queue against themselves.
    ///
    /// Returns how many were evicted, for the log.
    pub fn evict_other_sessions_for_user(&self, user_id: Uuid, keep: Uuid) -> Vec<Uuid> {
        let mut locked = self.map.lock().unwrap();
        let doomed: Vec<Uuid> = locked
            .iter()
            .filter(|(id, s)| **id != keep && s.user_id == user_id)
            .map(|(id, _)| *id)
            .collect();
        for id in &doomed {
            locked.remove(id);
        }
        doomed
    }

    pub fn store_new_session(&self, session: Arc<Session>) -> Uuid {
        let now_instant = Instant::now();
        let clear_before_instant = now_instant - self.ttl;
        let uuid_to_clear_before = self.get_uuid_for_instant(&clear_before_instant);

        let mut id = self.get_uuid_for_instant(&now_instant);
        {
            let mut locked = self.map.lock().unwrap();

            while locked.get(&id).is_some() {
                id = self.get_uuid_for_instant(&now_instant);
            }
            locked.insert(id.clone(), session);

            while let Some((k, _v)) = locked.first_key_value()
                && k < &uuid_to_clear_before
            {
                locked.pop_first();
            }
        }
        return id;
    }
}

#[derive(diesel::QueryableByName)]
struct SessionRow {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    user_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    secret_user_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    extra_secret: Uuid,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    expires_at_secs: i64,
}

/// Persist a freshly-created session so it survives an arena-server restart (the
/// `sessions` migration). Best-effort: a DB hiccup must NOT fail login — the session
/// still works in-memory this run; only cross-restart survival is lost.
pub async fn persist_session(db: &DbPool, session_id: Uuid, session: &Session) {
    use diesel_async::RunQueryDsl; // scoped here so it doesn't shadow AtomicU64::load in `sync`
    let mut conn = match db.get().await {
        Ok(c) => c,
        Err(_) => {
            error!("sessions: db pool unavailable (persist {session_id})");
            return;
        }
    };
    // The table is a restart-survival cache, not history. Production had 1,274
    // expired rows out of 1,296 because lookups filtered them but nothing ever
    // removed them. New-session creation is the natural bounded cleanup point.
    if let Err(e) = diesel::sql_query("DELETE FROM sessions WHERE expires_at <= now()")
        .execute(&mut conn)
        .await
    {
        error!("sessions: expired-row cleanup failed: {e}");
    }
    if let Err(e) = diesel::sql_query(
        "INSERT INTO sessions (session_id, user_id, secret_user_id, extra_secret, expires_at) \
         VALUES ($1, $2, $3, $4, to_timestamp($5)) ON CONFLICT (session_id) DO NOTHING",
    )
    .bind::<diesel::sql_types::Uuid, _>(session_id)
    .bind::<diesel::sql_types::Uuid, _>(session.user_id)
    .bind::<diesel::sql_types::Uuid, _>(session.secret_user_id)
    .bind::<diesel::sql_types::Uuid, _>(session.extra_secret)
    .bind::<diesel::sql_types::BigInt, _>(session.expire_unix_timestamp as i64)
    .execute(&mut conn)
    .await
    {
        error!("sessions: persist insert failed ({session_id}): {e}");
    }
}

/// Reconstruct a session from the `sessions` table on a cold lookup (after a restart
/// emptied the in-memory map). Filters expired rows. request_count resets to 1;
/// matchmaking_ws is re-established when the client reconnects the rms WebSocket.
async fn load_persisted_session(db: &DbPool, session_id: Uuid) -> Option<Session> {
    use diesel_async::RunQueryDsl; // scoped (see persist_session)
    let mut conn = db.get().await.ok()?;
    let row: SessionRow = diesel::sql_query(
        "SELECT user_id, secret_user_id, extra_secret, \
         CAST(EXTRACT(epoch FROM expires_at) AS BIGINT) AS expires_at_secs \
         FROM sessions WHERE session_id = $1 AND expires_at > now()",
    )
    .bind::<diesel::sql_types::Uuid, _>(session_id)
    .get_result(&mut conn)
    .await
    .ok()?;
    Some(Session {
        user_id: row.user_id,
        secret_user_id: row.secret_user_id,
        extra_secret: row.extra_secret,
        expire_unix_timestamp: row.expires_at_secs.max(0) as u64,
        request_count: AtomicU64::new(1),
        matchmaking_ws: Mutex::new(None),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncResponse {
    request_index: u64,
}

#[get("/blades.bgs.services/api/game/v1/public/sync")]
async fn sync(session: SessionLookedUpMaybe) -> Result<web::Json<SyncResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    Ok(web::Json(SyncResponse {
        request_index: session.session.current_request_index(),
    }))
}


/// Make `session_id` the only live session for this user, in memory and in the
/// `sessions` table.
///
/// Called on every path that mints a session. The DB half matters because
/// sessions are repopulated from that table after a restart — evicting only the
/// in-memory copy would bring the displaced device's session back to life on the
/// next deploy.
///
/// Best-effort on the DB, like `persist_session`: losing the row means the old
/// session could survive a restart, which is a far smaller problem than failing
/// a login.
pub async fn claim_account_for_this_device(
    store: &SessionStore,
    db: &DbPool,
    user_id: Uuid,
    session_id: Uuid,
) {
    let evicted = store.evict_other_sessions_for_user(user_id, session_id);
    if evicted.is_empty() {
        return;
    }
    log::info!(
        "session: user {user_id} signed in on a new device — evicted {} other live session(s); \
         the displaced device will reload from the start on its next request (no login needed)",
        evicted.len(),
    );

    use diesel_async::RunQueryDsl;
    let mut conn = match db.get().await {
        Ok(c) => c,
        Err(e) => {
            log::warn!("session: could not evict old sessions from the database: {e}");
            return;
        }
    };
    if let Err(e) = diesel::sql_query("DELETE FROM sessions WHERE user_id = $1 AND session_id <> $2")
        .bind::<diesel::sql_types::Uuid, _>(user_id)
        .bind::<diesel::sql_types::Uuid, _>(session_id)
        .execute(&mut conn)
        .await
    {
        log::warn!("session: could not evict old sessions from the database: {e}");
    }
}

/// One device owns the account.
///
/// Owner's rule, 2026-09-23: "I can play on device 1, go to main menu, lock
/// screen, then on device 2 open the app, load and play, lock screen. Then
/// opening device 1 automatically loads from the beginning, but it does not
/// need a login, or character transfer."
///
/// Dropping the displaced session is what produces that: its next request 401s,
/// the client re-authenticates on its own, and anonymous login resolves the
/// claimed device back to the same account.
#[cfg(test)]
mod one_device_owns_the_account {
    use super::{Session, SessionStore};
    use std::{sync::Arc, time::Duration};
    use uuid::Uuid;

    fn store() -> SessionStore {
        SessionStore::new(Duration::from_secs(3600))
    }

    /// Sessions are inserted under ids we choose rather than through
    /// `store_new_session`, on purpose.
    ///
    /// That function derives the id from `now - time_base` and prunes anything
    /// older than `now - ttl`. In a test the store is created microseconds
    /// before the session, so both encode second 0 and the comparison falls
    /// through to the random half of the uuid — a session can prune itself the
    /// moment it is stored. (That is a real, if narrow, edge for the first
    /// second after a server restart; it is not what these tests are about, and
    /// fixing it is a separate change.)
    fn add(s: &SessionStore, id: Uuid, user: Uuid) -> Uuid {
        s.insert_existing(
            id,
            Arc::new(Session::new(user, Uuid::new_v4(), Duration::from_secs(3600))),
        );
        id
    }

    /// The scenario, end to end: device 1, then device 2, then device 1 again.
    #[test]
    fn the_newest_device_wins_and_the_older_one_is_dropped() {
        let s = store();
        let user = Uuid::new_v4();

        let device1 = add(&s, Uuid::new_v4(), user);
        assert!(s.get(device1).is_some(), "device 1 is live after signing in");

        let device2 = add(&s, Uuid::new_v4(), user);
        s.evict_other_sessions_for_user(user, device2);

        assert!(s.get(device2).is_some(), "the newest device keeps its session");
        assert!(
            s.get(device1).is_none(),
            "the older device's session is gone — its next request 401s and it reloads"
        );

        // And back again: picking device 1 up displaces device 2, symmetrically.
        let device1_again = add(&s, Uuid::new_v4(), user);
        s.evict_other_sessions_for_user(user, device1_again);
        assert!(s.get(device1_again).is_some());
        assert!(s.get(device2).is_none());
    }

    /// CONTROL, and the one that would make this a catastrophe if wrong: a login
    /// must only ever evict the SAME user. Getting this backwards would sign out
    /// the whole server on every login.
    #[test]
    fn another_players_session_is_never_touched() {
        let s = store();
        let (me, them) = (Uuid::new_v4(), Uuid::new_v4());
        let their_session = add(&s, Uuid::new_v4(), them);
        let my_first = add(&s, Uuid::new_v4(), me);
        let my_second = add(&s, Uuid::new_v4(), me);

        let evicted = s.evict_other_sessions_for_user(me, my_second);

        assert_eq!(evicted, vec![my_first], "only my own older session goes");
        assert!(s.get(their_session).is_some(), "the other player is untouched");
        assert!(s.get(my_second).is_some());
    }

    /// Signing in when nothing else is live evicts nothing — so the log line
    /// only appears when a device was actually displaced.
    #[test]
    fn a_first_login_displaces_nobody() {
        let s = store();
        let user = Uuid::new_v4();
        let only = add(&s, Uuid::new_v4(), user);
        assert!(s.evict_other_sessions_for_user(user, only).is_empty());
        assert!(s.get(only).is_some());
    }

    /// Three devices, one account: after the third signs in, exactly one lives.
    #[test]
    fn only_ever_one_session_survives() {
        let s = store();
        let user = Uuid::new_v4();
        let a = add(&s, Uuid::new_v4(), user);
        let b = add(&s, Uuid::new_v4(), user);
        let c = add(&s, Uuid::new_v4(), user);
        let evicted = s.evict_other_sessions_for_user(user, c);
        assert_eq!(evicted.len(), 2);
        for gone in [a, b] {
            assert!(s.get(gone).is_none());
        }
        assert!(s.get(c).is_some());
    }

    /// THE SEAM. The eviction is useless if the login paths do not call it, and
    /// they compile perfectly well without. Every path that mints a session must
    /// claim the account for that device.
    #[test]
    fn every_login_path_claims_the_account() {
        let src = include_str!("authentification.rs");
        let mints = src.matches("store_new_session(").count();
        let claims = src.matches("claim_account_for_this_device(").count();
        assert!(mints > 0, "there is at least one login path");
        assert!(
            claims >= mints,
            "every path that mints a session must claim the account for that device \
             ({mints} mint(s), {claims} claim(s))"
        );
    }

    /// The database half is not optional: sessions are repopulated from the
    /// `sessions` table after a restart, so evicting only the in-memory copy
    /// would resurrect the displaced device on the next deploy.
    #[test]
    fn the_eviction_also_clears_the_persisted_row() {
        let src = include_str!("session.rs");
        let body = src
            .split("pub async fn claim_account_for_this_device(")
            .nth(1)
            .expect("the helper exists");
        assert!(body.contains("DELETE FROM sessions WHERE user_id = $1 AND session_id <> $2"));
    }
}
