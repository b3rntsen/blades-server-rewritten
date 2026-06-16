//! Matchmaker actor + the matchmaking REST surface.
//!
//! Flow (confirmed from captured prod traffic):
//!   1. client POSTs `matches/create` → we mint a ticketId, enqueue it, and
//!      return `{match:{ticketId,status:"QUEUED",port:0}}`.
//!   2. the matchmaker pushes three frames over the client's RMS WebSocket:
//!      `Searching` → `PotentialMatch` → `Succeeded{address,port,...}`.
//!   3. (cancellation) client POSTs `matches/{ticketId}/cancel` → `null`.
//!
//! v1 is solo + bot: a single ticket forms a match immediately and `Succeeded`
//! points at our configured arena UDP endpoint. Real pairing + the live UDP
//! match instance land in milestone (c)/(d).

use std::sync::Arc;
use std::time::Duration;

use actix_web::{
    HttpResponse, post,
    http::StatusCode,
    web::{self, Json},
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::RunQueryDsl;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use uuid::Uuid;

use crate::{
    BladeApiError, DbPool, ServerGlobal,
    arena::{MatchmakingMessage, config::ArenaConfig, match_registry::MatchRegistry},
    models::CharacterDbEntryCharacterWalletInventory,
    schema::characters,
    session::SessionLookedUpMaybe,
};

/// A queued matchmaking ticket handed to the matchmaker actor. Carries a clone
/// of the requesting session's RMS sender so the matchmaker can push frames
/// back to exactly that client.
pub struct TicketRequest {
    pub ticket_id: Uuid,
    pub user_id: Uuid,
    pub rms: UnboundedSender<MatchmakingMessage>,
}

/// Status of a recorded matchmaking ticket, for the web /arena activity feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RecentStatus {
    /// Queued, waiting for an opponent (or the solo-fallback timer).
    Searching,
    /// Resolved into a match (solo/bot or a PvP pair).
    Matched,
}

/// A JSON view of a recent ticket (what the dev `recent-matches` endpoint — and
/// thus the web /arena page — sees). The requesting user is shown only as an
/// opaque short tag (first 8 hex of the arena user id), never full identity.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentTicketView {
    pub ticket_id: Uuid,
    pub user_tag: String,
    pub status: RecentStatus,
    pub paired: bool,
    pub game_session_id: Option<Uuid>,
    pub age_seconds: u64,
    /// True when this ticket's user == the `userId` query filter (i.e. "you").
    pub mine: bool,
}

/// A row of the durable `arena_matches` table (migration 2026-06-16_add_arena_matches),
/// read back for the `recent-matches` endpoint. `age_seconds` is computed in SQL
/// (`now() - recorded_at`) so it survives restarts — unlike the old in-memory Instant.
#[derive(diesel::QueryableByName)]
struct ArenaMatchRow {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    ticket_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    user_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    paired: bool,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    game_session_id: Option<Uuid>,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    age_seconds: i64,
}

/// Record a newly-queued ticket as `searching` in `arena_matches`. Best-effort:
/// matchmaking must not block on (or fail from) the DB, so pool/SQL errors are
/// logged and swallowed. No-op when `db` is None (the unit test has no DB).
async fn record_match_queued(db: &Option<DbPool>, ticket_id: Uuid, user_id: Uuid) {
    let Some(db) = db else { return };
    let Ok(mut conn) = db.get().await else {
        warn!("arena_matches: db pool unavailable (queued {ticket_id})");
        return;
    };
    if let Err(e) = diesel::sql_query(
        "INSERT INTO arena_matches (ticket_id, user_id, status, recorded_at) \
         VALUES ($1, $2, 'searching', now()) ON CONFLICT (ticket_id) DO NOTHING",
    )
    .bind::<diesel::sql_types::Uuid, _>(ticket_id)
    .bind::<diesel::sql_types::Uuid, _>(user_id)
    .execute(&mut conn)
    .await
    {
        warn!("arena_matches: insert failed ({ticket_id}): {e}");
    }
}

/// Mark a ticket `matched` (solo or paired) in `arena_matches`. Best-effort.
async fn record_match_resolved(
    db: &Option<DbPool>,
    ticket_id: Uuid,
    game_session_id: Uuid,
    paired: bool,
) {
    let Some(db) = db else { return };
    let Ok(mut conn) = db.get().await else { return };
    let _ = diesel::sql_query(
        "UPDATE arena_matches SET status='matched', game_session_id=$2, paired=$3, \
         resolved_at=now() WHERE ticket_id=$1",
    )
    .bind::<diesel::sql_types::Uuid, _>(ticket_id)
    .bind::<diesel::sql_types::Uuid, _>(game_session_id)
    .bind::<diesel::sql_types::Bool, _>(paired)
    .execute(&mut conn)
    .await;
}

/// Load a player's combat loadout (equipped abilities + weapon damage enchants)
/// from their character row. Best-effort: returns a starter loadout when there's
/// no DB (the unit-test path), no character row, or a query error — matchmaking
/// must never fail on this.
async fn load_loadout(db: &Option<DbPool>, user_id: Uuid) -> crate::arena::combat::Loadout {
    use crate::arena::combat::loadout;
    let Some(db) = db else {
        return loadout::starter();
    };
    let Ok(mut conn) = db.get().await else {
        return loadout::starter();
    };
    let row = characters::table
        .filter(characters::user_id.eq(user_id))
        .select(CharacterDbEntryCharacterWalletInventory::as_select())
        .load(&mut conn)
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next());
    match row {
        Some(r) => loadout::from_character(&r.character.0, &r.inventory.0),
        None => loadout::starter(),
    }
}

/// Newest-first view of `arena_matches`, capped at `limit`, marking `mine`
/// against `filter`. Backs the dev `recent-matches` endpoint; durable across
/// restarts (#NB-3). Returns empty on a DB error (the endpoint stays up).
pub async fn query_recent_matches(
    db: &DbPool,
    limit: i64,
    filter: Option<Uuid>,
) -> Vec<RecentTicketView> {
    let Ok(mut conn) = db.get().await else { return Vec::new() };
    let rows: Vec<ArenaMatchRow> = diesel::sql_query(
        "SELECT ticket_id, user_id, status, paired, game_session_id, \
         CAST(EXTRACT(epoch FROM (now() - recorded_at)) AS BIGINT) AS age_seconds \
         FROM arena_matches ORDER BY recorded_at DESC LIMIT $1",
    )
    .bind::<diesel::sql_types::BigInt, _>(limit)
    .get_results(&mut conn)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(|r| {
            let id_str = r.user_id.to_string();
            RecentTicketView {
                ticket_id: r.ticket_id,
                user_tag: id_str[..8].to_string(),
                status: if r.status == "matched" {
                    RecentStatus::Matched
                } else {
                    RecentStatus::Searching
                },
                paired: r.paired,
                game_session_id: r.game_session_id,
                age_seconds: r.age_seconds.max(0) as u64,
                mine: filter.map(|f| f == r.user_id).unwrap_or(false),
            }
        })
        .collect()
}

/// Shared arena state (hung off `ServerGlobal`). Cloning the `UnboundedSender`
/// is the only thing request handlers touch — the queue itself lives inside the
/// single-owner matchmaker task.
pub struct ArenaGlobal {
    pub config: ArenaConfig,
    pub matchmaker_tx: UnboundedSender<TicketRequest>,
    pub registry: Arc<MatchRegistry>,
}

impl ArenaGlobal {
    /// Build the arena subsystem and spawn the matchmaker actor on the current
    /// arbiter. Returns the shared handle to store in `ServerGlobal`.
    pub fn start(config: ArenaConfig, db_pool: DbPool) -> Arc<Self> {
        let registry = MatchRegistry::new(config.max_concurrent_matches);

        let (tx, rx) = unbounded_channel::<TicketRequest>();
        let mm_cfg = config.clone();
        let mm_reg = registry.clone();
        actix_web::rt::spawn(async move {
            matchmaker_loop(rx, mm_cfg, mm_reg, Some(db_pool)).await;
        });
        // The live ENet arena host (tokio-enet) is spawned from main() once
        // ServerGlobal exists (it needs the shared Arc). `udp.rs`'s raw-socket
        // UdpServer is the dev/test reference for the crypto + FSM unit tests.

        Arc::new(ArenaGlobal {
            config,
            matchmaker_tx: tx,
            registry,
        })
    }
}

/// The matchmaker actor. Single owner of the ticket queue — no locks. v1 solo +
/// bot: each ticket resolves immediately to our configured UDP endpoint.
/// Seconds a lone ticket waits for an opponent before falling back to a solo
/// (no-opponent / bot) match, so a single tester isn't stuck "Searching".
const SOLO_FALLBACK_SECS: u64 = 15;

async fn matchmaker_loop(
    mut rx: UnboundedReceiver<TicketRequest>,
    config: ArenaConfig,
    registry: Arc<MatchRegistry>,
    db: Option<DbPool>,
) {
    info!(
        "matchmaker: started (advertise {}:{}, max {} matches)",
        config.advertise_host, config.udp_port, registry.max_matches
    );

    // A single ticket held while it waits for an opponent to pair with.
    let mut waiting: Option<TicketRequest> = None;
    loop {
        // If a ticket is already waiting, race the next ticket against a fallback
        // timer; otherwise just block for the next ticket.
        let next = if waiting.is_some() {
            tokio::select! {
                r = rx.recv() => r,
                _ = tokio::time::sleep(Duration::from_secs(SOLO_FALLBACK_SECS)) => {
                    let lone = waiting.take().expect("waiting is some");
                    info!("matchmaker: no opponent for ticket {} — solo fallback", lone.ticket_id);
                    resolve(&registry, &config, &db, &[lone]).await;
                    continue;
                }
            }
        } else {
            rx.recv().await
        };
        let Some(req) = next else { break };

        info!("matchmaker: ticket {} (user {})", req.ticket_id, req.user_id);
        record_match_queued(&db, req.ticket_id, req.user_id).await;
        // Push the captured 3-frame progression's first two frames now; the
        // `Succeeded` frame follows once the match resolves (pair or fallback).
        let _ = req
            .rms
            .send(MatchmakingMessage::Searching { ticket_id: req.ticket_id });
        let _ = req
            .rms
            .send(MatchmakingMessage::PotentialMatch { ticket_id: req.ticket_id });

        match waiting.take() {
            // A second player arrived → pair the two into ONE shared match.
            Some(first) => resolve(&registry, &config, &db, &[first, req]).await,
            // First player → hold it and wait for an opponent (or the timer above).
            None => waiting = Some(req),
        }
    }
    warn!("matchmaker: queue closed, actor exiting");
}

/// Allocate ONE match for these tickets (1 = solo/bot, 2 = a PvP pair) and push
/// `MatchmakingSucceeded` to each — all sharing one `gameSessionId`, each with its
/// own `playerSessionId` (the id the UDP layer admits it under).
async fn resolve(
    registry: &MatchRegistry,
    config: &ArenaConfig,
    db: &Option<DbPool>,
    tickets: &[TicketRequest],
) {
    let game_session_id = Uuid::new_v4();
    let paired = tickets.len() >= 2;
    let psids: Vec<String> = tickets
        .iter()
        .map(|_| format!("psess-{}", Uuid::new_v4()))
        .collect();

    // Load each player's combat loadout (equipped abilities + damage enchants)
    // from their character; falls back to a starter loadout without a DB/row.
    let mut loadouts = Vec::with_capacity(tickets.len());
    for t in tickets {
        loadouts.push(load_loadout(db, t.user_id).await);
    }
    if !registry.allocate(&psids, loadouts, game_session_id) {
        for t in tickets {
            warn!(
                "matchmaker: at capacity — ticket {} left unresolved",
                t.ticket_id
            );
        }
        return;
    }

    for (t, psid) in tickets.iter().zip(psids.iter()) {
        let succeeded = MatchmakingMessage::Succeeded {
            ticket_id: t.ticket_id,
            player_session_id: psid.clone(),
            game_session_id,
            address: config.advertise_host.clone(),
            port: config.udp_port,
        };
        if t.rms.send(succeeded).is_err() {
            // The match's capacity permit is held until both players connect; an
            // abandoned ticket leaks one slot until expiry (TODO: deadline sweep).
            warn!(
                "matchmaker: ticket {} — client RMS gone before Succeeded",
                t.ticket_id
            );
        }
        // Record the resolution so the web /arena page can show "matched".
        record_match_resolved(db, t.ticket_id, game_session_id, paired).await;
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // accepted for wire-compat; not used by the v1 solo+bot matcher
pub struct CreateMatchRequest {
    #[serde(default)]
    player_id: Option<Uuid>,
    #[serde(default)]
    fleet_key: Option<String>,
    #[serde(default)]
    player_region_latencies: Option<serde_json::Value>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CreateMatchResponse {
    r#match: MatchTicket,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct MatchTicket {
    ticket_id: Uuid,
    status: &'static str,
    port: u16,
}

#[post("/blades.bgs.services/api/matchmaking/v1/public/matches/create")]
pub async fn create_match(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    _body: web::Json<CreateMatchRequest>,
) -> Result<Json<CreateMatchResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let ticket_id = Uuid::new_v4();

    // The RMS WebSocket must already be open — the client holds it from login.
    // Clone the sender out under a brief lock, then drop the guard.
    let rms = {
        let guard = session.session.matchmaking_ws.lock().await;
        guard.clone()
    }
    .ok_or_else(|| BladeApiError::new(StatusCode::CONFLICT, 4, 1))?;

    app_state
        .arena
        .matchmaker_tx
        .send(TicketRequest {
            ticket_id,
            user_id: session.session.user_id,
            rms,
        })
        .map_err(|_| BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, 4, 2))?;

    Ok(Json(CreateMatchResponse {
        r#match: MatchTicket {
            ticket_id,
            status: "QUEUED",
            port: 0,
        },
    }))
}

#[post("/blades.bgs.services/api/matchmaking/v1/public/matches/{ticket_id}/cancel")]
pub async fn cancel_match(
    path: web::Path<Uuid>,
    session: SessionLookedUpMaybe,
) -> Result<HttpResponse, BladeApiError> {
    let _session = session.get_session_or_error()?;
    info!("matchmaker: cancel ticket {}", path.into_inner());
    // Captured behavior: 200 with a literal `null` body. v1 resolves tickets
    // immediately, so cancellation is an acknowledged no-op.
    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body("null"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::config::ArenaConfig;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    async fn next_succeeded(rx: &mut UnboundedReceiver<MatchmakingMessage>) -> (Uuid, String) {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("matchmaker frame timed out")
                .expect("rms channel closed");
            if let MatchmakingMessage::Succeeded {
                game_session_id,
                player_session_id,
                ..
            } = msg
            {
                return (game_session_id, player_session_id);
            }
        }
    }

    /// Two tickets enqueued back-to-back must be PAIRED into one match: both
    /// clients get `Succeeded` with the SAME gameSessionId and DISTINCT
    /// playerSessionIds (Gap 3).
    #[tokio::test]
    async fn pairs_two_tickets_into_one_match() {
        let registry = MatchRegistry::new(4);
        let config = ArenaConfig {
            advertise_host: "127.0.0.1".into(),
            udp_port: 7777,
            max_concurrent_matches: 4,
            max_queued_players: 64,
        };
        let (tx, rx) = unbounded_channel::<TicketRequest>();
        tokio::spawn(matchmaker_loop(rx, config, registry.clone(), None));

        let (rms_a, mut recv_a) = unbounded_channel();
        let (rms_b, mut recv_b) = unbounded_channel();
        tx.send(TicketRequest {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            rms: rms_a,
        })
        .unwrap();
        tx.send(TicketRequest {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            rms: rms_b,
        })
        .unwrap();

        let (gsid_a, psid_a) = next_succeeded(&mut recv_a).await;
        let (gsid_b, psid_b) = next_succeeded(&mut recv_b).await;
        assert_eq!(gsid_a, gsid_b, "both players share one gameSessionId");
        assert_ne!(psid_a, psid_b, "each player gets a distinct playerSessionId");
        // The paired match is allocated and holds one capacity permit.
        assert_eq!(registry.available_permits(), 3);
    }
}
