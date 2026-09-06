use std::{collections::HashMap, sync::Arc};

use actix_web::{HttpRequest, http::StatusCode, post, web};
use blades_lib::user_data::UserAccount;
use diesel::{
    ExpressionMethods, OptionalExtension, QueryDsl, SelectableHelper, associations::HasTable,
    insert_into,
};
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, json_db::JsonDbWrapper, models::UserDBEntry, schema,
    session::Session,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnonLoginInfo {
    user_id: Option<String>,
    // The retail client sends `deviceId: null` on a first anon login (no
    // GPGS/device identity yet, e.g. a fresh emulator). Must be Option or serde
    // rejects null with a 400 deserialize error before the handler even runs.
    device_id: Option<String>,
    platform: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponse {
    session: SessionResponseInner,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionResponseInner {
    session_id: String,
    user_id: String,
    token: String,
    schema: String,
    /// Retail returns this on anon, login AND link; the client keeps it to
    /// re-establish a session without asking for the password again. Ours is
    /// the session token, which is what the client already treats as opaque.
    login_token: String,
    feature_status: u64,
    linked_accounts_status: u64,
    token_expiration_seconds: u64,
    denied_features: HashMap<String, DeniedFeatureResponse>,
}

impl SessionResponseInner {
    fn from_session(session_id: Uuid, session: &Session) -> Self {
        let mut denied_features = HashMap::new();
        denied_features.insert(
            "e3_signup_bonus".to_string(),
            DeniedFeatureResponse {
                deny_expired_secs: 0,
                deny_reason_code: 1,
            },
        );

        SessionResponseInner {
            session_id: session_id.to_string(),
            user_id: session.secret_user_id.to_string(),
            token: session.generate_token(&session_id),
            schema: "blades_v1".to_string(),
            login_token: session.generate_token(&session_id),
            feature_status: 7,
            // Session.LinkedAccountsStatus bitmask (client dump.cs:484710). The client's
            // "Do you want to sign in?" AnonymousWarning nag (shown at spend/commit points
            // like the customization FINISH) fires when the account is ONLY
            // BNET_ANONYMOUS_ACCOUNT(4). We force anon login (no Bethesda/Google, and their
            // servers are gone), so 4 reproduced the nag. Report BNET_GAME_ACCOUNT(8)
            // instead — the "has a real account" family retail returned when it did NOT nag
            // (captured: gc=9=1|8, bnet=16, link=24 all suppress it; anon=4 shows it). The
            // session itself is unaffected; this only tells the client it's linked so it
            // stops nudging. No Bethesda contact. [investigated 2026-07-04]
            linked_accounts_status: 8,
            token_expiration_seconds: session.expire_unix_timestamp,
            denied_features,
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeniedFeatureResponse {
    deny_expired_secs: u64,
    deny_reason_code: u64,
}

/// `POST /…/auth/bnet/login` — sign in with the username and password the
/// player set on their profile.
///
/// This is what makes ONE generic APK work for everybody. Identity is currently
/// the WireGuard IP the mitm stamps into `X-Newblades-Device-Ip` (the client
/// sends `deviceId: null`), so a VPN-free build would otherwise hand every
/// player a brand-new anonymous character.
///
/// The body shape is retail's own, read off a capture rather than invented:
/// `{username, password, deviceId, platform}`, answered with the same
/// `SessionResponse` as `auth/anon`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BnetLoginRequest {
    username: String,
    password: String,
    #[serde(default)]
    #[allow(dead_code)]
    device_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    platform: Option<String>,
}

#[post("/blades.bgs.services/api/authentication/v1/public/auth/bnet/login")]
async fn bnet_log_in(
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<BnetLoginRequest>,
) -> Result<web::Json<SessionResponse>, BladeApiError> {
    let body = body.into_inner();
    let username = crate::credentials::normalise_username(&body.username);
    let mut conn = app_state.db_pool.get().await.unwrap();

    let found = crate::credentials::find_by_username(&mut conn, &username)
        .await
        .unwrap_or(None);

    // One 401 for "no such user" AND "wrong password", and the password is
    // verified even when the row is missing. Answering faster for an unknown
    // username turns this endpoint into a way to enumerate who plays here.
    let (user_id, hash) = match found {
        Some(row) => (Some(row.user_id), row.password_hash),
        None => (
            None,
            // A well-formed hash of a value nobody can supply, so the failure
            // path does the same PBKDF2 work as the success path.
            "pbkdf2$200000$00000000000000000000000000000000$             0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
        ),
    };
    let ok = crate::credentials::verify_password(&body.password, &hash);
    let Some(user_id) = user_id.filter(|_| ok) else {
        // Service id 3 is what the rest of this module reports auth failures
        // under (see the NOT_FOUND below); 110 is this endpoint's code.
        return Err(BladeApiError::new(StatusCode::UNAUTHORIZED, 3, 110));
    };

    let user: UserDBEntry = {
        use crate::schema::users::dsl as u;
        u::users
            .filter(u::id.eq(user_id))
            .select(UserDBEntry::as_select())
            .first(&mut conn)
            .await
            .map_err(|_| BladeApiError::new(StatusCode::UNAUTHORIZED, 3, 111))?
    };

    let session = Arc::new(Session::new(
        user.id,
        user.secret_id,
        app_state.session_store.ttl,
    ));
    let session_id = app_state.session_store.store_new_session(session.clone());
    crate::session::persist_session(&app_state.db_pool, session_id, session.as_ref()).await;
    log::info!("account login: {username} -> user {}", user.id);
    Ok(web::Json(SessionResponse {
        session: SessionResponseInner::from_session(session_id, session.as_ref()),
    }))
}

/// `POST /…/auth/bnet/link` and `/auth/bnet/link/force` — Settings → "Link with
/// a Bethesda.net account", pointed at the login the player set on our website.
///
/// WHY THIS IS THE RIGHT DOOR
///
/// The obvious way to reach a login screen is to un-rig `Flow.StartAuthentication`
/// so the client shows its own sign-in. That does not work: a cold start fires
/// `AuthenticateNoUi` -> PreFtue -> Google Play Games, and when GPGS cannot sign
/// in the client falls straight back to `auth/anon` without ever offering a
/// username field. Measured on a real Pixel — `silentSignIn.onFailure`, then a
/// POST to `auth/anon`.
///
/// The link flow has no such gate. The player boots anonymously (which already
/// works with no VPN and no certificate), opens Settings, and types the username
/// and password from their profile. `CreateLinkAccountToPlatformRequest` takes
/// `bnetUserName` and `bnetPassword` directly (dump.cs:459421).
///
/// THE WIRE FORMAT IS RETAIL'S, read off 19 successful captures:
///
///   request  {username, password, selectedUserId, language}
///   link     {accountLinkResult: {conflict, session?, conflictingUserIds?, loginToken}}
///   force    {session}
///
/// `conflict` is the normal case for us, not an error: the player is signed in
/// as a throwaway anonymous account and their credential names a DIFFERENT one
/// — the account with their real character. The client then shows its own
/// "which account do you want?" prompt and calls `/force`. Answering `conflict:
/// false` and silently switching would be a lie the client cannot undo.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BnetLinkRequest {
    username: String,
    password: String,
    /// The account the client is currently signed in as — the anonymous one it
    /// would abandon by linking. Absent or unparseable is treated as "none",
    /// which reports a conflict; that is the safe direction, because it asks
    /// rather than assumes.
    #[serde(default)]
    selected_user_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    language: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountLinkResult {
    conflict: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<SessionResponseInner>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflicting_user_ids: Option<Vec<String>>,
    login_token: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BnetLinkResponse {
    account_link_result: AccountLinkResult,
}

/// Verify the credential and mint a session for the account it names.
///
/// Shared by link and link/force so the two cannot drift: the only difference
/// between them is what the caller does with the result, never who is
/// authenticated or how.
async fn resolve_link(
    app_state: &Arc<ServerGlobal>,
    username_raw: &str,
    password: &str,
) -> Result<(Uuid, Uuid, Arc<Session>), BladeApiError> {
    let username = crate::credentials::normalise_username(username_raw);
    let mut conn = app_state.db_pool.get().await.unwrap();

    let found = crate::credentials::find_by_username(&mut conn, &username)
        .await
        .unwrap_or(None);

    // Same constant-work failure path as bnet_log_in: an unknown username must
    // not be faster than a wrong password, or this endpoint enumerates players.
    let (user_id, hash) = match found {
        Some(row) => (Some(row.user_id), row.password_hash),
        None => (
            None,
            "pbkdf2$200000$00000000000000000000000000000000$             0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
        ),
    };
    let ok = crate::credentials::verify_password(password, &hash);
    let Some(user_id) = user_id.filter(|_| ok) else {
        return Err(BladeApiError::new(StatusCode::UNAUTHORIZED, 3, 112));
    };

    let user: UserDBEntry = {
        use crate::schema::users::dsl as u;
        u::users
            .filter(u::id.eq(user_id))
            .select(UserDBEntry::as_select())
            .first(&mut conn)
            .await
            .map_err(|_| BladeApiError::new(StatusCode::UNAUTHORIZED, 3, 113))?
    };

    let session = Arc::new(Session::new(
        user.id,
        user.secret_id,
        app_state.session_store.ttl,
    ));
    Ok((user.id, user.secret_id, session))
}

/// Is the account the client is already signed in as the same one the
/// credential names?
///
/// The comparison is against the SECRET id, not the row id. The secret id is
/// the only user id the client is ever told (`SessionResponseInner` reports
/// `secret_user_id`), so comparing `selectedUserId` against the row id would
/// report a conflict every single time — including when the player is already
/// signed in as exactly the account they are linking.
///
/// Anything unparseable, or absent, counts as "not the same". That direction is
/// deliberate: it makes the client ASK which account to keep instead of
/// silently switching the player to another one.
fn is_same_account(selected: Option<&str>, secret_id: Uuid) -> bool {
    selected
        .and_then(|s| Uuid::parse_str(s).ok())
        .is_some_and(|s| s == secret_id)
}

#[post("/blades.bgs.services/api/authentication/v1/public/auth/bnet/link")]
async fn bnet_link(
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<BnetLinkRequest>,
) -> Result<web::Json<BnetLinkResponse>, BladeApiError> {
    let body = body.into_inner();
    let (user_id, secret_id, session) =
        resolve_link(&app_state, &body.username, &body.password).await?;

    // The client names the account it is currently signed in as by its SECRET
    // id — that is the only user id it is ever told (see `SessionResponseInner`,
    // which reports `secret_user_id`). Comparing it against the row id would
    // report a conflict every single time.
    let same_account = is_same_account(body.selected_user_id.as_deref(), secret_id);

    if !same_account {
        // Do NOT mint a session here. The client is about to ask the player
        // which account to keep; handing it a live session for the other one
        // before they answer would switch them without consent.
        log::info!(
            "account link: conflict — credential names {secret_id}, client is signed in as {:?}",
            body.selected_user_id
        );
        return Ok(web::Json(BnetLinkResponse {
            account_link_result: AccountLinkResult {
                conflict: true,
                session: None,
                conflicting_user_ids: Some(vec![secret_id.to_string()]),
                login_token: String::new(),
            },
        }));
    }

    let session_id = app_state.session_store.store_new_session(session.clone());
    crate::session::persist_session(&app_state.db_pool, session_id, session.as_ref()).await;
    let inner = SessionResponseInner::from_session(session_id, session.as_ref());
    let login_token = inner.login_token.clone();
    log::info!("account link: user {user_id} linked (no conflict)");
    Ok(web::Json(BnetLinkResponse {
        account_link_result: AccountLinkResult {
            conflict: false,
            session: Some(inner),
            conflicting_user_ids: None,
            login_token,
        },
    }))
}

/// `POST /…/auth/bnet/link/force` — the player answered the conflict prompt and
/// chose the linked account. Same credential check; the anonymous account they
/// were using is simply left behind (never deleted — it may hold a character
/// they later want, and deleting on a menu tap is not recoverable).
#[post("/blades.bgs.services/api/authentication/v1/public/auth/bnet/link/force")]
async fn bnet_link_force(
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<BnetLinkRequest>,
) -> Result<web::Json<SessionResponse>, BladeApiError> {
    let body = body.into_inner();
    let (user_id, _secret_id, session) =
        resolve_link(&app_state, &body.username, &body.password).await?;
    let session_id = app_state.session_store.store_new_session(session.clone());
    crate::session::persist_session(&app_state.db_pool, session_id, session.as_ref()).await;
    log::info!("account link (forced): now signed in as user {user_id}");
    Ok(web::Json(SessionResponse {
        session: SessionResponseInner::from_session(session_id, session.as_ref()),
    }))
}

#[post("/blades.bgs.services/api/authentication/v1/public/auth/anon")]
async fn anon_log_in(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    info: web::Json<AnonLoginInfo>,
) -> Result<web::Json<SessionResponse>, BladeApiError> {
    use schema::users::dsl::*;

    // Per-player claim link. Record this device's anon login (so the web claim UI
    // can list "recent devices"), and if the device has already been claimed
    // (bound to a user via /api/dev/v1/bind-device), log in as THAT user — their
    // Transfer'd character. Binding takes precedence over the dev-login override
    // below: claimed devices get their own character, unclaimed ones still fall
    // back to dev-login (no regression). device_bindings (migration
    // 2026-06-08_add_device_bindings) is queried with raw SQL to avoid a
    // timestamp-typed diesel schema (no chrono feature needed).
    // Effective device key: the client-sent deviceId, or — for the fork client,
    // which sends deviceId: null — the WG peer IP the arena_redirect addon tags
    // (X-Newblades-Device-Ip). Each newblades WG peer has a unique, stable IP, so
    // it serves as a per-device identity for the claim link. (A client with
    // neither still falls through to the dev-login / create path below.)
    //
    // The source WG peer IP is always extracted (when present) so it can be
    // cross-linked with stable-hash bindings via the `source_wg_ip` column — see
    // Fix 1 / migration 2026-06-21-000000-0000_device_bindings_wg_ip. This
    // bridges the gap when a device was bound under its stable deviceId hash but
    // later connects with deviceId: null (e.g. after reinstalling the rigged APK).
    let source_wg_ip: Option<String> = req
        .headers()
        .get("x-newblades-device-ip")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let effective_device_id: Option<String> = info.0.device_id.clone().or_else(|| source_wg_ip.clone());
    if let Some(device_id_val) = effective_device_id {
        let mut conn = app_state.db_pool.get().await.unwrap();
        // Upsert the device_bindings row. Also write `source_wg_ip` when the
        // header is present — this lets stable-hash-keyed bindings be found via
        // the secondary WG-IP lookup below (Fix 1 systemic binding fix).
        let _ = diesel::sql_query(
            "INSERT INTO device_bindings (device_id, platform, last_seen, source_wg_ip) \
             VALUES ($1, $2, now(), $3) \
             ON CONFLICT (device_id) DO UPDATE SET last_seen = now(), \
             platform = COALESCE(EXCLUDED.platform, device_bindings.platform), \
             source_wg_ip = COALESCE(EXCLUDED.source_wg_ip, device_bindings.source_wg_ip)",
        )
        .bind::<diesel::sql_types::Text, _>(device_id_val.clone())
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(Some(info.0.platform.clone()))
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(source_wg_ip.clone())
        .execute(&mut conn)
        .await;

        #[derive(diesel::QueryableByName)]
        struct BoundUser {
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            user_id: Uuid,
        }
        // Primary lookup: by the effective device key (stable hash or WG IP).
        let bound: Option<BoundUser> = diesel::sql_query(
            "SELECT user_id FROM device_bindings WHERE device_id = $1 AND user_id IS NOT NULL",
        )
        .bind::<diesel::sql_types::Text, _>(device_id_val.clone())
        .get_result(&mut conn)
        .await
        .optional()
        .unwrap_or(None);
        // Secondary lookup (Fix 1): when the effective key is a WG IP and the
        // primary lookup found nothing, try to find a stable-hash binding whose
        // `source_wg_ip` matches this IP. This fires when a device was bound
        // under its real deviceId hash but now reconnects with deviceId: null
        // (rigged APK reinstall, server restart, etc.).  Take the most recently
        // active binding so a re-claim moves the binding correctly.
        let bound: Option<BoundUser> = if bound.is_none() {
            if let Some(ref wg_ip) = source_wg_ip {
                let secondary = diesel::sql_query(
                    "SELECT user_id FROM device_bindings \
                     WHERE source_wg_ip = $1 AND user_id IS NOT NULL \
                     ORDER BY last_seen DESC LIMIT 1",
                )
                .bind::<diesel::sql_types::Text, _>(wg_ip.clone())
                .get_result::<BoundUser>(&mut conn)
                .await
                .optional()
                .unwrap_or(None);
                if secondary.is_some() {
                    log::info!(
                        "device_bindings: WG-IP fallback resolved {} via source_wg_ip {} \
                         (primary key miss — device reconnected with null deviceId after \
                         being bound under a stable hash; Fix 1 systemic binding fix)",
                        device_id_val, wg_ip
                    );
                }
                secondary
            } else {
                None
            }
        } else {
            bound
        };
        if let Some(b) = bound {
            let result = users
                .select(UserDBEntry::as_select())
                .filter(id.eq(b.user_id))
                .load(&mut conn)
                .await
                .unwrap();
            if let Some(user) = result.get(0) {
                let session = Arc::new(Session::new(
                    user.id,
                    user.secret_id,
                    app_state.session_store.ttl,
                ));
                let session_id = app_state.session_store.store_new_session(session.clone());
                crate::session::persist_session(&app_state.db_pool, session_id, session.as_ref()).await;
                return Ok(web::Json(SessionResponse {
                    session: SessionResponseInner::from_session(session_id, session.as_ref()),
                }));
            }
            // Bound to a now-missing user — fall through to the normal flow.
        }
    }

    // Dev override (ARENA_DEV_LOGIN_USER_ID): resolve EVERY anon login to one
    // configured user — so a freshly-installed client lands on a Transfer'd
    // character. There is no Bethesda/Google identity on this server to map a
    // device to; see ServerGlobal.dev_login_user_id. Unset in normal operation.
    if let Some(dev_uid) = app_state.dev_login_user_id {
        let mut conn = app_state.db_pool.get().await.unwrap();
        let result = users
            .select(UserDBEntry::as_select())
            .filter(id.eq(dev_uid))
            .load(&mut conn)
            .await
            .unwrap();
        let user = match result.get(0) {
            Some(v) => v,
            None => return Err(BladeApiError::new(StatusCode::NOT_FOUND, 3, 101)),
        };
        let session = Arc::new(Session::new(
            user.id,
            user.secret_id,
            app_state.session_store.ttl,
        ));
        let session_id = app_state.session_store.store_new_session(session.clone());
        crate::session::persist_session(&app_state.db_pool, session_id, session.as_ref()).await;
        // A brand-new anon user has no character, and the shipped APK has FTUE
        // patched out — so without this it would ask for its characters, get an
        // empty list and sit on the loading screen forever. Best-effort: a
        // failure here must not break the login itself.
        if let Err(e) = crate::character::ensure_starter_character(&app_state, session.user_id).await {
            log::warn!("could not provision a starter character for {}: {}", session.user_id, e);
        }
        return Ok(web::Json(SessionResponse {
            session: SessionResponseInner::from_session(session_id, session.as_ref()),
        }));
    }

    if let Some(private_user_id) = info.0.user_id {
        // load pre-existing user
        // http code 404 service 3 error code 101 if not found, apparently
        let mut conn = app_state.db_pool.get().await.unwrap();
        let private_user_id = match Uuid::try_parse(&private_user_id) {
            Ok(v) => v,
            Err(_e) => return Err(BladeApiError::new(StatusCode::NOT_FOUND, 3, 101)),
        };

        let result = users
            .select(UserDBEntry::as_select())
            .filter(secret_id.eq(private_user_id))
            .load(&mut conn)
            .await
            .unwrap();
        let user = if let Some(v) = result.get(0) {
            v
        } else {
            return Err(BladeApiError::new(StatusCode::NOT_FOUND, 3, 101)); // user not found
        };

        //TODO: some actual form of authentification.
        let session = Arc::new(Session::new(
            user.id,
            user.secret_id,
            app_state.session_store.ttl,
        ));
        let session_id = app_state.session_store.store_new_session(session.clone());
        crate::session::persist_session(&app_state.db_pool, session_id, session.as_ref()).await;
        // A brand-new anon user has no character, and the shipped APK has FTUE
        // patched out — so without this it would ask for its characters, get an
        // empty list and sit on the loading screen forever. Best-effort: a
        // failure here must not break the login itself.
        if let Err(e) = crate::character::ensure_starter_character(&app_state, session.user_id).await {
            log::warn!("could not provision a starter character for {}: {}", session.user_id, e);
        }
        return Ok(web::Json(SessionResponse {
            session: SessionResponseInner::from_session(session_id, session.as_ref()),
        }));
    } else {
        // create a new user
        let mut new_user = UserAccount::new_random();
        if info.0.platform == "gp" {
            if let Some(did) = info.0.device_id {
                new_user.gp_deviceids.insert(did);
            }
        } else {
            return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 3, 3)); //INVALID_REQUEST_DEVICE_ID
        }
        let new_user_id = Uuid::new_v4();
        let new_user_secret_id = Uuid::new_v4();
        let mut conn = app_state.db_pool.get().await.unwrap();
        insert_into(users::table())
            .values(UserDBEntry {
                id: new_user_id,
                secret_id: new_user_secret_id,
                data: JsonDbWrapper(new_user),
            })
            .execute(&mut conn)
            .await
            .unwrap();

        let session = Arc::new(Session::new(
            new_user_id,
            new_user_secret_id,
            app_state.session_store.ttl,
        ));
        let session_id = app_state.session_store.store_new_session(session.clone());
        crate::session::persist_session(&app_state.db_pool, session_id, session.as_ref()).await;
        // A brand-new anon user has no character, and the shipped APK has FTUE
        // patched out — so without this it would ask for its characters, get an
        // empty list and sit on the loading screen forever. Best-effort: a
        // failure here must not break the login itself.
        if let Err(e) = crate::character::ensure_starter_character(&app_state, session.user_id).await {
            log::warn!("could not provision a starter character for {}: {}", session.user_id, e);
        }
        return Ok(web::Json(SessionResponse {
            session: SessionResponseInner::from_session(session_id, session.as_ref()),
        }));
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    /// The conflict decision is the whole of the link flow's judgement, and it
    /// hinges on comparing the right id. Getting this wrong is invisible in a
    /// build: every link would simply report a conflict, the client would show
    /// its "which account?" prompt every time, and it would look like the
    /// player's own doing rather than a bug.
    #[test]
    fn compares_against_the_secret_id_the_client_was_given() {
        let secret = Uuid::from_u128(0xAAAA);
        let row_id = Uuid::from_u128(0xBBBB);
        assert!(is_same_account(Some(&secret.to_string()), secret));
        assert!(
            !is_same_account(Some(&row_id.to_string()), secret),
            "the row id is never what the client holds"
        );
    }

    #[test]
    fn anything_unknown_reports_a_conflict_rather_than_assuming() {
        let secret = Uuid::from_u128(0xAAAA);
        for selected in [None, Some(""), Some("not-a-uuid"), Some("0")] {
            assert!(
                !is_same_account(selected, secret),
                "{selected:?} must not be treated as a match"
            );
        }
    }

    #[test]
    fn a_different_account_is_a_conflict() {
        // The normal case for us: signed in anonymously, linking to the account
        // that actually holds their character.
        let anon = Uuid::from_u128(1);
        let real = Uuid::from_u128(2);
        assert!(!is_same_account(Some(&anon.to_string()), real));
    }

    /// Uuid formatting is case-insensitive on parse; a client that upper-cases
    /// its own id must not be told it is a different account.
    #[test]
    fn case_does_not_change_the_answer() {
        let secret = Uuid::from_u128(0xABCDEF);
        assert!(is_same_account(Some(&secret.to_string().to_uppercase()), secret));
    }
}
