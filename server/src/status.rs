use std::sync::Arc;
use std::sync::atomic::Ordering;

use actix_web::{get, post, web, HttpRequest, HttpResponse};
use diesel_async::RunQueryDsl;
use serde::Serialize;

use crate::ServerGlobal;
use crate::arena::matchmaker::MatchmakerCommand;

const ARENA_DRAIN_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

async fn resume_matchmaker(state: &ServerGlobal) -> bool {
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    if state
        .arena
        .matchmaker_tx
        .send(MatchmakerCommand::Resume { ack: ack_tx })
        .is_err()
    {
        return false;
    }

    matches!(
        tokio::time::timeout(ARENA_DRAIN_ACK_TIMEOUT, ack_rx).await,
        Ok(Ok(()))
    )
}

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
        Ok(HealthProbe { schema_ready: true }) => {
            let active_matches = state.arena.registry.active_count();
            HttpResponse::Ok().json(serde_json::json!({
                "ok": true,
                "database": "ok",
                "arenaActiveMatches": active_matches,
                "arenaAcceptingMatches": std::sync::atomic::AtomicBool::load(
                    &state.arena.accepting_matches,
                    Ordering::Acquire,
                ),
            }))
        }
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

/// Put the old process into deployment-drain mode. Token-gated because this makes
/// matchmaking temporarily unavailable. The actor acknowledgement is essential: by
/// the time this returns, every unresolved ticket has received `MatchmakingFailed`
/// and the queue is empty; only `arenaActiveMatches` still needs to reach zero.
#[post("/healthz/arena-drain")]
pub async fn arena_drain(
    req: HttpRequest,
    state: web::Data<Arc<ServerGlobal>>,
) -> Result<HttpResponse, crate::BladeApiError> {
    crate::admin::check_import_token(&state, &req)?;
    state
        .arena
        .accepting_matches
        .store(false, Ordering::Release);

    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    if state
        .arena
        .matchmaker_tx
        .send(MatchmakerCommand::Drain { ack: ack_tx })
        .is_err()
    {
        return Ok(HttpResponse::ServiceUnavailable().json(serde_json::json!({
            "ok": false,
            "error": "matchmaker-unavailable",
        })));
    }

    match tokio::time::timeout(ARENA_DRAIN_ACK_TIMEOUT, ack_rx).await {
        Ok(Ok(failed_tickets)) => Ok(HttpResponse::Ok().json(serde_json::json!({
            "ok": true,
            "failedQueuedTickets": failed_tickets,
            "activeMatches": state.arena.registry.active_count(),
        }))),
        _ => {
            if resume_matchmaker(&state).await {
                state.arena.accepting_matches.store(true, Ordering::Release);
            }
            Ok(HttpResponse::ServiceUnavailable().json(serde_json::json!({
                "ok": false,
                "error": "drain-ack-timeout",
            })))
        }
    }
}

/// Re-open matchmaking when a pending deployment is deferred or fails before the
/// process is replaced. A successful replacement does not need this: the new process
/// starts accepting matches by default.
#[post("/healthz/arena-resume")]
pub async fn arena_resume(
    req: HttpRequest,
    state: web::Data<Arc<ServerGlobal>>,
) -> Result<HttpResponse, crate::BladeApiError> {
    crate::admin::check_import_token(&state, &req)?;
    if resume_matchmaker(&state).await {
        state.arena.accepting_matches.store(true, Ordering::Release);
        Ok(HttpResponse::Ok().json(serde_json::json!({ "ok": true })))
    } else {
        state
            .arena
            .accepting_matches
            .store(false, Ordering::Release);
        Ok(HttpResponse::ServiceUnavailable().json(serde_json::json!({
            "ok": false,
            "error": "matchmaker-unavailable",
        })))
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
