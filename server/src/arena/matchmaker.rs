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
use log::{info, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    arena::{MatchmakingMessage, config::ArenaConfig, match_registry::MatchRegistry},
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
    pub fn start(config: ArenaConfig) -> Arc<Self> {
        let registry = MatchRegistry::new(config.max_concurrent_matches);

        let (tx, rx) = unbounded_channel::<TicketRequest>();
        let mm_cfg = config.clone();
        let mm_reg = registry.clone();
        actix_web::rt::spawn(async move {
            matchmaker_loop(rx, mm_cfg, mm_reg).await;
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
                    resolve(&registry, &config, &[lone]);
                    continue;
                }
            }
        } else {
            rx.recv().await
        };
        let Some(req) = next else { break };

        info!("matchmaker: ticket {} (user {})", req.ticket_id, req.user_id);
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
            Some(first) => resolve(&registry, &config, &[first, req]),
            // First player → hold it and wait for an opponent (or the timer above).
            None => waiting = Some(req),
        }
    }
    warn!("matchmaker: queue closed, actor exiting");
}

/// Allocate ONE match for these tickets (1 = solo/bot, 2 = a PvP pair) and push
/// `MatchmakingSucceeded` to each — all sharing one `gameSessionId`, each with its
/// own `playerSessionId` (the id the UDP layer admits it under).
fn resolve(registry: &MatchRegistry, config: &ArenaConfig, tickets: &[TicketRequest]) {
    let game_session_id = Uuid::new_v4();
    let psids: Vec<String> = tickets
        .iter()
        .map(|_| format!("psess-{}", Uuid::new_v4()))
        .collect();

    if !registry.allocate(&psids, game_session_id) {
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
        tokio::spawn(matchmaker_loop(rx, config, registry.clone()));

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
