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
        request_index: session
            .session
            .request_count
            .load(Ordering::Relaxed)
            .saturating_sub(1), // the counter is incremented before processing the variable. This may cause issue if multiple request from the client are made simulteneously, thought.
    }))
}
