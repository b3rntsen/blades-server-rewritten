use std::sync::Arc;

use actix_web::{get, web, HttpResponse};
use diesel_async::RunQueryDsl;
use serde::Serialize;

use crate::ServerGlobal;

#[derive(diesel::QueryableByName)]
struct HealthProbe {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    schema_ready: bool,
}

/// Operational health, deliberately separate from the retail status payload.
/// A 200 proves both the HTTP process and its PostgreSQL pool can execute a
/// query; pool/schema failures become 503 so Docker and deploy checks fail.
#[get("/healthz")]
async fn healthz(state: web::Data<Arc<ServerGlobal>>) -> HttpResponse {
    let Ok(mut conn) = state.db_pool.get().await else {
        return HttpResponse::ServiceUnavailable().json(serde_json::json!({
            "ok": false,
            "database": "pool-unavailable"
        }));
    };
    match diesel::sql_query("SELECT to_regclass('public.users') IS NOT NULL AS schema_ready")
        .get_result::<HealthProbe>(&mut conn)
        .await
    {
        Ok(HealthProbe { schema_ready: true }) => HttpResponse::Ok().json(serde_json::json!({
            "ok": true,
            "database": "ok"
        })),
        Ok(_) => HttpResponse::ServiceUnavailable().json(serde_json::json!({
            "ok": false,
            "database": "schema-not-ready"
        })),
        Err(error) => {
            log::error!("healthz: database query failed: {error}");
            HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "ok": false,
                "database": "query-failed"
            }))
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    ttl: u64,
    systems: Vec<StatusEntryResponse>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusEntryResponse {
    name: &'static str,
    status: &'static str,
}

#[get("announcements.blades.bgs.services/status/status.json")]
async fn check_status() -> web::Json<StatusResponse> {
    web::Json(StatusResponse {
        ttl: 300,
        systems: vec![
            StatusEntryResponse {
                name: "authentication",
                status: "online",
            },
            StatusEntryResponse {
                name: "game",
                status: "online",
            },
            StatusEntryResponse {
                name: "pvp",
                status: "online",
            },
            StatusEntryResponse {
                name: "guilds",
                status: "online",
            },
            StatusEntryResponse {
                name: "events",
                status: "online",
            },
            StatusEntryResponse {
                name: "social",
                status: "online",
            },
            StatusEntryResponse {
                name: "quests",
                status: "online",
            },
            StatusEntryResponse {
                name: "challenges",
                status: "online",
            },
            StatusEntryResponse {
                name: "shops",
                status: "online",
            },
        ],
    })
}
