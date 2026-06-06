//! Match registry — the capacity-bounded table linking matchmaker-issued
//! matches to live UDP peers.
//!
//! The matchmaker `allocate`s a slot (bounded by a `Semaphore` =
//! `ArenaConfig.max_concurrent_matches`), keyed by the `playerSessionId` it
//! advertises in `MatchmakingSucceeded`. When that client connects over UDP and
//! presents the id, the UDP layer `admit`s it: X25519 ECDH completes, the match
//! moves to `active`, and the capacity permit is held for the match's lifetime
//! (released when the `ActiveMatch` is dropped on disconnect).
//!
//! Concurrency: hot state is `std::sync::Mutex<HashMap>` locked only for short,
//! synchronous critical sections — never across an `.await` (the single UDP
//! demux task and the matchmaker task are the only callers). The `Semaphore` is
//! the cap gauge; `try_acquire_owned` gives clean reject-when-full.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use log::{info, warn};
use rand::RngExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use arena_proto::{
    CryptoCtx, first_opcode_in_plaintext, reconstruct_plaintext, x25519_public, x25519_shared,
};

/// A match the matchmaker reserved, awaiting the client's UDP connect.
struct PendingMatch {
    game_session_id: Uuid,
    permit: OwnedSemaphorePermit,
}

/// A connected match: the agreed crypto context + the held capacity permit.
struct ActiveMatch {
    #[allow(dead_code)]
    game_session_id: Uuid,
    #[allow(dead_code)]
    player_session_id: String,
    crypto: CryptoCtx,
    _permit: OwnedSemaphorePermit, // released on drop → frees a Semaphore slot
}

pub struct MatchRegistry {
    semaphore: Arc<Semaphore>,
    pending: Mutex<HashMap<String, PendingMatch>>, // key = player_session_id
    active: Mutex<HashMap<SocketAddr, ActiveMatch>>,
    pub max_matches: usize,
}

impl MatchRegistry {
    pub fn new(max_matches: usize) -> Arc<Self> {
        Arc::new(MatchRegistry {
            semaphore: Arc::new(Semaphore::new(max_matches)),
            pending: Mutex::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
            max_matches,
        })
    }

    /// Matchmaker: reserve a capacity slot for a new match, keyed by the
    /// `playerSessionId` it will advertise. Returns false at capacity.
    pub fn allocate(&self, player_session_id: String, game_session_id: Uuid) -> bool {
        match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => {
                self.pending
                    .lock()
                    .unwrap()
                    .insert(player_session_id, PendingMatch { game_session_id, permit });
                true
            }
            Err(_) => {
                warn!(
                    "match registry: at capacity ({} concurrent matches)",
                    self.max_matches
                );
                false
            }
        }
    }

    /// UDP: a client presents its issued `playerSessionId` + X25519 pubkey. If a
    /// pending match matches, complete ECDH, move it to `active`, and return the
    /// `(server_pubkey, nonce)` for the handshake reply. `None` ⇒ unknown id.
    pub fn admit(
        &self,
        peer: SocketAddr,
        player_session_id: &str,
        client_pub: &[u8; 32],
    ) -> Option<([u8; 32], [u8; 8])> {
        let pending = self.pending.lock().unwrap().remove(player_session_id)?;
        let (server_sk, server_pk) = gen_keypair();
        let key = x25519_shared(&server_sk, client_pub);
        let nonce = gen_nonce();
        self.active.lock().unwrap().insert(
            peer,
            ActiveMatch {
                game_session_id: pending.game_session_id,
                player_session_id: player_session_id.to_string(),
                crypto: CryptoCtx { key, nonce },
                _permit: pending.permit,
            },
        );
        info!("match registry: admitted {peer} (psess {player_session_id})");
        Some((server_pk, nonce))
    }

    /// Decode an inbound datagram from an active peer → first GameMessageId.
    pub fn decode(&self, peer: &SocketAddr, datagram: &[u8]) -> Option<u8> {
        let active = self.active.lock().unwrap();
        let m = active.get(peer)?;
        let pt = reconstruct_plaintext(datagram, &m.crypto.key, &m.crypto.nonce, None, false)?;
        first_opcode_in_plaintext(&pt)
    }

    pub fn is_active(&self, peer: &SocketAddr) -> bool {
        self.active.lock().unwrap().contains_key(peer)
    }

    /// Drop a peer's match (disconnect) → releases its capacity permit.
    pub fn remove(&self, peer: &SocketAddr) {
        if self.active.lock().unwrap().remove(peer).is_some() {
            info!("match registry: removed {peer}");
        }
    }

    pub fn active_count(&self) -> usize {
        self.active.lock().unwrap().len()
    }
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

/// Random 32-byte X25519 secret + its public key (X25519 clamps internally).
pub(crate) fn gen_keypair() -> ([u8; 32], [u8; 32]) {
    let sk: [u8; 32] = rand::rng().random();
    let pk = x25519_public(&sk);
    (sk, pk)
}

/// Random 8-byte nonce (ChaCha20 counter stays 0; all variation is in the nonce).
pub(crate) fn gen_nonce() -> [u8; 8] {
    rand::rng().random()
}
