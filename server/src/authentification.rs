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
            feature_status: 7,
            linked_accounts_status: 4,
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
    let effective_device_id: Option<String> = info.0.device_id.clone().or_else(|| {
        req.headers()
            .get("x-newblades-device-ip")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    });
    if let Some(device_id_val) = effective_device_id {
        let mut conn = app_state.db_pool.get().await.unwrap();
        let _ = diesel::sql_query(
            "INSERT INTO device_bindings (device_id, platform, last_seen) VALUES ($1, $2, now()) \
             ON CONFLICT (device_id) DO UPDATE SET last_seen = now(), \
             platform = COALESCE(EXCLUDED.platform, device_bindings.platform)",
        )
        .bind::<diesel::sql_types::Text, _>(device_id_val.clone())
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(Some(info.0.platform.clone()))
        .execute(&mut conn)
        .await;

        #[derive(diesel::QueryableByName)]
        struct BoundUser {
            #[diesel(sql_type = diesel::sql_types::Uuid)]
            user_id: Uuid,
        }
        let bound: Option<BoundUser> = diesel::sql_query(
            "SELECT user_id FROM device_bindings WHERE device_id = $1 AND user_id IS NOT NULL",
        )
        .bind::<diesel::sql_types::Text, _>(device_id_val)
        .get_result(&mut conn)
        .await
        .optional()
        .unwrap_or(None);
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
        return Ok(web::Json(SessionResponse {
            session: SessionResponseInner::from_session(session_id, session.as_ref()),
        }));
    }
}
