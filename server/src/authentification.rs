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
    /// re-establish the account without asking for the password again. It is
    /// the user's stable secret id, not the short-lived session token.
    login_token: String,
    feature_status: u64,
    linked_accounts_status: u64,
    token_expiration_seconds: u64,
    denied_features: HashMap<String, DeniedFeatureResponse>,
}

/// The durable bearer the client stores as its BNet login token.
///
/// It must be the same value in a completed session and in the intermediate
/// `accountLinkResult`. `LinkAccountResponse.GetNewLoginToken` reads the latter
/// before the conflict picker continues to `/force`; returning a throwaway
/// session token there makes the next cold start present a token this server
/// can never resolve.
fn persistent_login_token(secret_user_id: Uuid) -> String {
    secret_user_id.to_string()
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
            // The client persists this value and sends it alone to bnet/login
            // on the next cold start. Retail's token is a UUID (capture 145996),
            // while our old `session_id|extra_secret` value was only understood
            // by Authorization middleware and expired with that session. The
            // secret user id is already the persistent bearer used by auth/anon
            // (`userId`), so this grants no new authority; it makes the BNet
            // route honour the same existing account secret.
            login_token: persistent_login_token(session.secret_user_id),
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
/// Both body shapes are retail's own, read off captures rather than invented:
/// an interactive sign-in sends `{username, password, deviceId, platform}`;
/// later cold starts send `{loginToken, deviceId, platform}`. Both are answered
/// with the same `SessionResponse` as `auth/anon`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BnetLoginRequest {
    /// Present for an interactive sign-in. On a later cold start the client
    /// sends neither field and supplies only `loginToken` instead.
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    /// The persistent UUID returned by the preceding login/link response.
    #[serde(default)]
    login_token: Option<String>,
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
    let mut conn = app_state.db_pool.get().await.unwrap();

    let (user_id, log_name) = if let (Some(username_raw), Some(password)) =
        (body.username.as_deref(), body.password.as_deref())
    {
        let username = crate::credentials::normalise_username(username_raw);
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
                crate::credentials::ENUMERATION_DUMMY_HASH.to_string(),
            ),
        };
        let ok = crate::credentials::verify_password(password, &hash);
        let Some(user_id) = user_id.filter(|_| ok) else {
            return Err(BladeApiError::new(StatusCode::UNAUTHORIZED, 3, 110));
        };
        (user_id, Some(username))
    } else if let Some(login_token) = body.login_token.as_deref() {
        // This is the normal cold-start request after a successful BNet link:
        // capture 145996 is exactly {loginToken, deviceId, platform}. The old
        // required username/password struct rejected it during deserialization,
        // so the client fell back to a fresh anonymous account on every launch.
        let token = Uuid::parse_str(login_token)
            .map_err(|_| BladeApiError::new(StatusCode::UNAUTHORIZED, 3, 110))?;
        let found: Option<Uuid> = {
            use crate::schema::users::dsl as u;
            u::users
                .filter(u::secret_id.eq(token))
                .select(u::id)
                .first(&mut conn)
                .await
                .optional()
                .unwrap_or(None)
        };
        let Some(user_id) = found else {
            return Err(BladeApiError::new(StatusCode::UNAUTHORIZED, 3, 110));
        };
        (user_id, None)
    } else {
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
    match log_name {
        Some(username) => log::info!("account login: {username} -> user {}", user.id),
        None => log::info!("account login: persisted token -> user {}", user.id),
    }
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
            crate::credentials::ENUMERATION_DUMMY_HASH.to_string(),
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
        // BOTH profiles, not just the one being linked to. This is the picker's
        // content: the client draws a row per id and asks which to KEEP — "the
        // other one will be discarded", in its own words. Retail's captures
        // carry two 36-char ids for exactly this reason.
        //
        // Sending only the linked id left the dialog with nothing to lay out:
        // the player saw "Another Blades profile was found" above two blank
        // rows and could not tap their way out. The client had even fetched the
        // other profile's characters successfully — it simply had no second
        // entry to render against.
        //
        // Secret ids, in the same currency as `selectedUserId`; the row id is
        // never something the client has seen.
        let mut ids = Vec::new();
        if let Some(sel) = body.selected_user_id.as_deref().and_then(|s| Uuid::parse_str(s).ok()) {
            ids.push(sel.to_string());
        }
        ids.push(secret_id.to_string());

        // The client exposes this field as `NewBnetToken` and persists it before
        // continuing through the conflict picker. It therefore has to be the
        // same durable bearer `/auth/bnet/login` accepts on a cold start. The old
        // value was a token for an unregistered throwaway session, so every
        // restart rejected it and fell back to anonymous login.
        let login_token = persistent_login_token(secret_id);

        // Still no SESSION. The client is about to ask which profile to keep;
        // handing it a live session for the other account before the player
        // answers would switch them without consent — and the loser of that
        // choice is discarded.
        log::info!(
            "account link: conflict — credential names {secret_id}, client is signed in as {:?}",
            body.selected_user_id
        );
        return Ok(web::Json(BnetLinkResponse {
            account_link_result: AccountLinkResult {
                conflict: true,
                session: None,
                conflicting_user_ids: Some(ids),
                login_token,
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

        return Ok(web::Json(SessionResponse {
            session: SessionResponseInner::from_session(session_id, session.as_ref()),
        }));
    } else {
        if info.0.platform != "gp" {
            return Err(BladeApiError::new(StatusCode::BAD_REQUEST, 3, 3)); //INVALID_REQUEST_DEVICE_ID
        }
        // A MISSING device id must NOT be fatal. This was
        //   let Some(this_device) = info.0.device_id.clone() else { return 400 };
        // and it locked every NEW player out of the standard VPN build, which
        // sends `deviceId: null` on purpose (identity comes from the WireGuard
        // peer IP the mitm stamps into X-Newblades-Device-Ip — see the note at
        // the top of this function). On a FIRST login every earlier path falls
        // through: `device_bindings` has a row but `user_id` is NULL until
        // somebody claims it, and `info.user_id` is None because the client has
        // no secret yet. So control reached here and answered
        // INVALID_REQUEST_DEVICE_ID — the player could never get an account.
        // Creating the user and merely skipping `gp_deviceids` is what this
        // replaced, and it was right.
        //
        // NOTE, because the obvious "improvement" is worse: the recognition
        // query below deliberately uses the REAL device id, not
        // `effective_device_id` (device id OR WG peer IP). Keying `gp_deviceids`
        // on a tunnel address would let a REASSIGNED peer IP hand a new player
        // somebody else's account — the same class of mistake as the claim-link
        // breach documented in web/lib/arena-claim.ts, where 10 of 70 bindings
        // ended up cross-user. The VPN build's identity already has a home in
        // `device_bindings`. So a VPN first login mints a fresh user here and the
        // player binds it once via the claim link; the duplicate-account problem
        // (#105) therefore remains for UNCLAIMED VPN devices, which is a real
        // gap, but the answer to it is claiming, not widening this lookup.
        let this_device = info.0.device_id.clone();

        // BEFORE minting anything: has this device been here before?
        //
        // We have always RECORDED `gp_deviceids` when creating a user, and never
        // once read it back. So the only ways to be recognised were a
        // device_bindings row or the client returning the secret it was handed --
        // and a client that does not keep that secret got a brand-new identity,
        // and a brand-new character, on every login (#105).
        //
        // It was not hypothetical: on the live database 7 device ids were shared
        // by more than one user, 33 duplicate accounts between them, one device
        // having minted 12.
        //
        // The device id is the identity of an ANONYMOUS account here, which is
        // the same trust already placed in `device_bindings` above; this adds no
        // new authority, it just uses the record we were already keeping. A
        // signed-in account is unaffected — that path returns earlier.
        // Only meaningful when the client gave us a real device id; see above.
        if let Some(ref this_device) = this_device {
            let mut conn = app_state.db_pool.get().await.unwrap();
            let existing: Vec<UserDBEntry> = users
                .select(UserDBEntry::as_select())
                .filter(
                    diesel::dsl::sql::<diesel::sql_types::Bool>(
                        "data->'gp_deviceids' @> ",
                    )
                    .bind::<diesel::sql_types::Jsonb, _>(serde_json::json!([this_device])),
                )
                .load(&mut conn)
                .await
                .unwrap_or_default();

            // Exactly one match is a returning device. Several means the damage
            // above already happened for this device; picking one arbitrarily
            // would hand out whichever character sorted first, so leave those to
            // be merged deliberately rather than guess.
            if existing.len() == 1 {
                let user = &existing[0];
                log::info!(
                    "anon login: device {} recognised as existing user {}",
                    this_device,
                    user.id
                );
                let session = Arc::new(Session::new(
                    user.id,
                    user.secret_id,
                    app_state.session_store.ttl,
                ));
                let session_id = app_state.session_store.store_new_session(session.clone());
                crate::session::persist_session(&app_state.db_pool, session_id, session.as_ref())
                    .await;
                // This is a completed login, not merely a lookup hint. Falling
                // through creates a second user for the same device and returns
                // that new identity instead of the session we just persisted.
                return Ok(web::Json(SessionResponse {
                    session: SessionResponseInner::from_session(session_id, session.as_ref()),
                }));
            } else if existing.len() > 1 {
                log::warn!(
                    "anon login: device {} matches {} users — not guessing which; \
                     minting a new one. These need merging.",
                    this_device,
                    existing.len()
                );
            }
        }

        // create a new user
        let mut new_user = UserAccount::new_random();
        // Nothing to record for a client that sends `deviceId: null` — recording
        // the WG peer IP here would make a reassigned address recognise the
        // wrong account on a later login.
        if let Some(dev) = this_device {
            new_user.gp_deviceids.insert(dev);
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

        return Ok(web::Json(SessionResponse {
            session: SessionResponseInner::from_session(session_id, session.as_ref()),
        }));
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;

    /// Report #108: after an interactive link the retail client cold-starts
    /// with only `{loginToken, deviceId, platform}`. Requiring username and
    /// password made serde reject that request before the handler ran.
    #[test]
    fn a_cold_bnet_login_with_only_the_persisted_token_deserializes() {
        let token = Uuid::from_u128(0xA11CE);
        let request: BnetLoginRequest = serde_json::from_value(serde_json::json!({
            "loginToken": token,
            "deviceId": "install-hash",
            "platform": "gp"
        }))
        .expect("retail's cold-start BNet request must be accepted");
        assert_eq!(request.login_token, Some(token.to_string()));
        assert!(request.username.is_none());
        assert!(request.password.is_none());
    }

    /// The value the client persists must survive sessions and server restarts.
    /// `secret_user_id` is already the account bearer accepted by auth/anon;
    /// the previous session-id token expired and was not accepted by bnet/login.
    #[test]
    fn login_token_is_the_stable_account_secret_not_the_session_token() {
        let account = Uuid::from_u128(0xACCC0A17);
        let session = Session::new(
            Uuid::from_u128(1),
            account,
            std::time::Duration::from_secs(3600),
        );
        let session_id = Uuid::from_u128(2);
        let response = SessionResponseInner::from_session(session_id, &session);

        assert_eq!(response.login_token, account.to_string());
        assert_ne!(response.login_token, session.generate_token(&session_id));
        assert!(Uuid::parse_str(&response.login_token).is_ok(), "retail loginToken is a UUID");
    }

    /// Report #125: the normal anonymous-to-real-account link reports a
    /// conflict before `/force`. The client persists this intermediate field as
    /// its new BNet token, so it must resolve to the same account after restart.
    #[test]
    fn conflict_login_token_is_the_same_durable_bearer_as_the_session() {
        let linked_account = Uuid::from_u128(0xBEE7_125);
        let token = persistent_login_token(linked_account);

        assert_eq!(token, linked_account.to_string());
        assert_eq!(Uuid::parse_str(&token), Ok(linked_account));
    }

    /// The conflict decision is the whole of the link flow's judgement, and it
    /// hinges on comparing the right id. Getting this wrong is invisible in a
    /// build: every link would simply report a conflict, the client would show
    /// its "which account?" prompt every time, and it would look like the
    /// player's own doing rather than a bug.
    /// What a device-id match means, by how many users it finds.
    ///
    /// #105: we recorded `gp_deviceids` on every anon signup and never read it
    /// back, so a client that did not keep its secret got a new identity — and a
    /// new character — every login. On the live database 7 devices were shared by
    /// more than one user, 33 duplicate accounts between them.
    ///
    /// The rule has to be careful in the ambiguous case: several matches means the
    /// damage already happened for that device, and picking one would hand over
    /// whichever character sorted first.
    #[test]
    fn a_device_is_only_reused_when_it_names_exactly_one_user() {
        #[derive(Debug, PartialEq)]
        enum Outcome { Create, Reuse, DoNotGuess }

        fn decide(matches: usize) -> Outcome {
            if matches == 1 { Outcome::Reuse }
            else if matches > 1 { Outcome::DoNotGuess }
            else { Outcome::Create }
        }

        assert_eq!(decide(0), Outcome::Create, "an unseen device gets a new account");
        assert_eq!(decide(1), Outcome::Reuse, "a returning device keeps its account");
        // the 2-user and 12-user devices measured on prod
        assert_eq!(decide(2), Outcome::DoNotGuess, "ambiguous devices must not be guessed");
        assert_eq!(decide(12), Outcome::DoNotGuess);
    }

    /// PR #212 removed the starter-character call and accidentally removed the
    /// return beside it. The handler then persisted a session for the recognised
    /// user, fell through, minted another user, and returned the new account.
    /// Pin the control-flow boundary because the match-count unit test above
    /// cannot see what the request handler does after making its decision.
    #[test]
    fn a_recognised_device_returns_before_new_user_creation() {
        let src = include_str!("authentification.rs");
        let start = src
            .find("if existing.len() == 1 {")
            .expect("unique-device branch");
        let end = src[start..]
            .find("} else if existing.len() > 1 {")
            .map(|offset| start + offset)
            .expect("ambiguous-device branch");
        let unique_branch = &src[start..end];

        assert!(
            unique_branch.contains("return Ok(web::Json(SessionResponse"),
            "recognised-device login must return its persisted session before the new-user path"
        );
    }

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


    /// The conflict payload IS the picker's content. Retail's captures carry
    /// TWO 36-char ids; we sent one, and the player got a dialog with two blank
    /// rows and no way out. This pins the shape so it cannot regress into that.
    #[test]
    fn a_conflict_lists_both_profiles_in_client_currency() {
        let selected = Uuid::from_u128(0x1111);
        let linked = Uuid::from_u128(0x2222);

        // Mirrors the handler's construction.
        let build = |sel: Option<&str>, linked: Uuid| {
            let mut ids = Vec::new();
            if let Some(s) = sel.and_then(|s| Uuid::parse_str(s).ok()) {
                ids.push(s.to_string());
            }
            ids.push(linked.to_string());
            ids
        };

        let ids = build(Some(&selected.to_string()), linked);
        assert_eq!(ids.len(), 2, "the picker needs a row per profile");
        assert_eq!(ids[0], selected.to_string(), "the current profile comes first");
        assert_eq!(ids[1], linked.to_string());
        // Retail's ids are plain 36-character UUID strings.
        assert!(ids.iter().all(|i| i.len() == 36), "must be bare uuid strings");

        // With nothing usable from the client, one row is still better than a
        // dialog that cannot be dismissed — but it must never be zero.
        for missing in [None, Some("not-a-uuid")] {
            let ids = build(missing, linked);
            assert_eq!(ids.len(), 1);
            assert_eq!(ids[0], linked.to_string());
        }
    }

    /// Uuid formatting is case-insensitive on parse; a client that upper-cases
    /// its own id must not be told it is a different account.
    #[test]
    fn case_does_not_change_the_answer() {
        let secret = Uuid::from_u128(0xABCDEF);
        assert!(is_same_account(Some(&secret.to_string().to_uppercase()), secret));
    }
}
