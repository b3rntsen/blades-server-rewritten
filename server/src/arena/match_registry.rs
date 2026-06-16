//! Match registry — the capacity-bounded table linking matchmaker-issued
//! matches to live UDP peers.
//!
//! A **match** holds up to `capacity` players (1 = solo/bot, 2 = a PvP pair) and
//! one authoritative [`MatchInstance`]. The matchmaker `allocate`s a match
//! (bounded by a `Semaphore` = `ArenaConfig.max_concurrent_matches`), registering
//! the `playerSessionId`(s) it advertises in `MatchmakingSucceeded` against the
//! match's `gameSessionId`. When a client connects over UDP and presents its id,
//! the UDP layer `admit`s it: X25519 ECDH completes and the player joins the
//! match. Inbound game messages drive the shared FSM, whose s2c output is
//! **relayed to whichever player the FSM targets** (self and/or opponent),
//! encrypted under that target's own key. The capacity permit is held for the
//! match's lifetime (released when the last player leaves and the match is dropped).
//!
//! Concurrency: hot state is `std::sync::Mutex<HashMap>` locked only for short,
//! synchronous critical sections — never across an `.await` (the single UDP demux
//! task and the matchmaker task are the only callers). The `Semaphore` is the cap
//! gauge; `try_acquire_owned` gives clean reject-when-full.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};
use rand::RngExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use arena_proto::{
    CryptoCtx, chacha20_legacy_xor, first_opcode_in_plaintext, reconstruct_plaintext,
    x25519_public, x25519_shared,
};

use crate::arena::combat::{Loadout, MatchInstance};

/// A match whose clients never finish connecting holds its capacity permit
/// (acquired by the matchmaker in `allocate`); without a sweep that slot leaks
/// until the process restarts — observed 2026-06-16 as the registry stuck
/// "at capacity (2 matches)" after a couple of failed connects. `sweep_expired`
/// reclaims such matches. Conservative first values from the 1–2-player tests
/// (clients that connect do so within seconds of `Succeeded`); easy to tune.
const CONNECT_DEADLINE: Duration = Duration::from_secs(45); // under-capacity → reclaim
const MATCH_MAX_AGE: Duration = Duration::from_secs(600); // absolute safety net

/// A connected player within a match: its peer address + the agreed crypto.
struct PlayerConn {
    addr: SocketAddr,
    #[allow(dead_code)]
    player_session_id: String,
    crypto: CryptoCtx,
}

/// A live match: up to `capacity` players sharing one authoritative instance and
/// one capacity permit (released when the match is dropped — i.e. last player out).
struct Match {
    game_session_id: Uuid,
    /// Allocation order — `admit_connection` (the real op-0x38 handshake carries
    /// no playerSessionId on the wire) FIFO-binds a connection to the oldest
    /// match with a free slot.
    order: u64,
    capacity: usize,
    players: Vec<PlayerConn>,
    instance: MatchInstance,
    /// When `allocate` reserved this match — `sweep_expired` reclaims abandoned
    /// matches (clients never connected) so their capacity permit can't leak.
    created_at: Instant,
    _permit: OwnedSemaphorePermit,
}

pub struct MatchRegistry {
    semaphore: Arc<Semaphore>,
    pending: Mutex<HashMap<String, Uuid>>, // player_session_id -> game_session_id
    matches: Mutex<HashMap<Uuid, Match>>,  // game_session_id -> Match
    addr_index: Mutex<HashMap<SocketAddr, Uuid>>, // connected peer -> its match
    next_order: std::sync::atomic::AtomicU64, // monotonic match-allocation order
    pub max_matches: usize,
}

impl MatchRegistry {
    pub fn new(max_matches: usize) -> Arc<Self> {
        Arc::new(MatchRegistry {
            semaphore: Arc::new(Semaphore::new(max_matches)),
            pending: Mutex::new(HashMap::new()),
            matches: Mutex::new(HashMap::new()),
            addr_index: Mutex::new(HashMap::new()),
            next_order: std::sync::atomic::AtomicU64::new(0),
            max_matches,
        })
    }

    /// Matchmaker: reserve ONE capacity slot for a new match and register the
    /// `playerSessionId`(s) it will advertise (1 = solo/bot, 2 = a paired PvP
    /// match) against `game_session_id`. Returns false at capacity.
    pub fn allocate(
        &self,
        player_session_ids: &[String],
        loadouts: Vec<Loadout>,
        game_session_id: Uuid,
    ) -> bool {
        self.allocate_with_bots(player_session_ids, loadouts, game_session_id, 0)
    }

    /// Like [`allocate`](Self::allocate), but the match gets `bots` extra
    /// server-driven fighters with NO UDP peer (a solo-vs-bot match). The combat
    /// instance has `real_peers + bots` FIGHTERS, but the round starts once the
    /// `real_peers` human peers connect (`expected_peers`) — the bot fighters are
    /// pre-present, so the match never hangs waiting for a peer that won't come.
    pub fn allocate_with_bots(
        &self,
        player_session_ids: &[String],
        loadouts: Vec<Loadout>,
        game_session_id: Uuid,
        bots: usize,
    ) -> bool {
        let permit = match self.semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!(
                    "match registry: at capacity ({} concurrent matches)",
                    self.max_matches
                );
                return false;
            }
        };
        // `capacity` = real-peer admit slots (a bot has no UDP peer); the combat
        // instance gets `capacity + bots` fighters but waits for only `capacity`
        // real peers (expected_peers) before starting the round.
        let capacity = player_session_ids.len().max(1);
        let fighters = capacity + bots;
        let order = self
            .next_order
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.matches.lock().unwrap().insert(
            game_session_id,
            Match {
                game_session_id,
                order,
                capacity,
                players: Vec::with_capacity(capacity),
                instance: MatchInstance::new(fighters, capacity, loadouts, Instant::now()),
                created_at: Instant::now(),
                _permit: permit,
            },
        );
        let mut pending = self.pending.lock().unwrap();
        for psid in player_session_ids {
            pending.insert(psid.clone(), game_session_id);
        }
        info!(
            "match registry: allocated match {game_session_id} ({capacity} peer slot(s), {fighters} fighter(s), {bots} bot(s)) — {} slot(s) free of {}",
            self.semaphore.available_permits(),
            self.max_matches
        );
        true
    }

    /// UDP: a client presents its issued `playerSessionId` + X25519 pubkey. If it
    /// belongs to a reserved match with a free slot, complete ECDH, add the player,
    /// and return the `(server_pubkey, nonce)` for the handshake reply. `None` ⇒
    /// unknown id or the match is full.
    pub fn admit(
        &self,
        peer: SocketAddr,
        player_session_id: &str,
        client_pub: &[u8; 32],
    ) -> Option<([u8; 32], [u8; 8])> {
        let gsid = *self.pending.lock().unwrap().get(player_session_id)?;
        let mut matches = self.matches.lock().unwrap();
        let m = matches.get_mut(&gsid)?;
        if m.players.len() >= m.capacity {
            warn!("match registry: match {gsid} full; rejecting {peer}");
            return None;
        }
        // Commit: the psid is consumed only once we know the match can take it.
        self.pending.lock().unwrap().remove(player_session_id);

        let (server_sk, server_pk) = gen_keypair();
        let key = x25519_shared(&server_sk, client_pub);
        let nonce = gen_nonce();
        m.players.push(PlayerConn {
            addr: peer,
            player_session_id: player_session_id.to_string(),
            crypto: CryptoCtx { key, nonce },
        });
        self.addr_index.lock().unwrap().insert(peer, gsid);
        info!(
            "match registry: admitted {peer} (psess {player_session_id}) into match {gsid} [{}/{}]",
            m.players.len(),
            m.capacity
        );
        Some((server_pk, nonce))
    }

    /// Live-host (real op-0x38 handshake) path. The retail connect handshake
    /// carries only the client's X25519 pubkey — the `playerSessionId` is NOT on
    /// the wire (it comes later, encrypted; spec §4.1/§9). So bind the connection
    /// to the **oldest reserved match with a free slot** (FIFO), complete ECDH, and
    /// return `(server_pubkey, nonce)` for the reply. `None` ⇒ no free slot.
    ///
    /// v1 limitation: with several concurrent pending matches this FIFO bind can
    /// misassign a connection (the disambiguating psid isn't on the wire yet). For
    /// the low-concurrency first release it's exact; precise binding (from the
    /// first decrypted PlayerInfo, or a per-match UDP port) is the refinement.
    pub fn admit_connection(
        &self,
        peer: SocketAddr,
        client_pub: &[u8; 32],
    ) -> Option<([u8; 32], [u8; 8])> {
        let mut matches = self.matches.lock().unwrap();
        let gsid = matches
            .values()
            .filter(|m| m.players.len() < m.capacity)
            .min_by_key(|m| m.order)
            .map(|m| m.game_session_id)?;
        let m = matches.get_mut(&gsid).expect("just selected");

        let (server_sk, server_pk) = gen_keypair();
        let key = x25519_shared(&server_sk, client_pub);
        let nonce = gen_nonce();
        m.players.push(PlayerConn {
            addr: peer,
            player_session_id: String::new(), // bound later if/when the psid arrives
            crypto: CryptoCtx { key, nonce },
        });
        if let Some(prev) = self.addr_index.lock().unwrap().insert(peer, gsid) {
            if prev != gsid {
                warn!(
                    "match registry: peer {peer} re-bound {prev} → {gsid} — possible \
                     docker-proxy SNAT source collision (two clients sharing one source addr)"
                );
            }
        }
        info!(
            "match registry: connection {peer} bound to match {gsid} [{}/{}]",
            m.players.len(),
            m.capacity
        );
        Some((server_pk, nonce))
    }

    /// Raw-socket dev path ([`udp::UdpServer`]). The whole ENet datagram is walked
    /// + decrypted here. Single-client harness: replies are ENet-framed and
    /// returned for the addressed peer. The live path is [`handle_live_user_data`].
    ///
    /// [`udp::UdpServer`]: crate::arena::udp::UdpServer
    /// [`handle_live_user_data`]: Self::handle_live_user_data
    pub fn handle_inbound(&self, peer: &SocketAddr, datagram: &[u8]) -> Option<InboundOutcome> {
        let gsid = *self.addr_index.lock().unwrap().get(peer)?;
        let mut matches = self.matches.lock().unwrap();
        let m = matches.get_mut(&gsid)?;
        let sender = m.players.iter().position(|p| &p.addr == peer)?;

        let pt = {
            let c = &m.players[sender].crypto;
            reconstruct_plaintext(datagram, &c.key, &c.nonce, None, false)
        };
        let opcode = pt.as_deref().and_then(first_opcode_in_plaintext);

        let now = Instant::now();
        let mut replies = Vec::new();
        if let Some(op) = opcode {
            // The raw dev path carries no message body, so synthesize a c2s
            // (marker ‖ op) — enough for opcode-only transitions (e.g. concede).
            // It can only answer the addressed peer (== all s2c for a solo match).
            let synth = [0x84u8, op];
            for (target, user_data) in m.instance.on_c2s(sender, &synth, now) {
                if target != sender {
                    continue;
                }
                let seq = m.instance.next_seq();
                let c = &m.players[sender].crypto;
                replies.push(crate::arena::udp::build_send_reliable(0, seq, c, &user_data));
            }
        }
        Some(InboundOutcome {
            opcode,
            replies,
            state: m.instance.state_name(),
        })
    }

    /// Live-host (rusty_enet) path. rusty_enet has already deframed the datagram,
    /// so `user_data` is the raw SEND payload = `chacha20(marker ‖ opcode ‖ body)`.
    /// Decrypt with the SENDER's key, drive the shared FSM, and return the s2c
    /// replies as `(target peer addr, encrypted user-data)` — each encrypted under
    /// the TARGET player's key, ready to hand to that peer's `Peer::send`. This is
    /// where opponent relay happens (A's action → B's stream). `None` ⇒ the peer
    /// is not in an active match.
    pub fn handle_live_user_data(
        &self,
        peer: &SocketAddr,
        user_data: &[u8],
    ) -> Option<LiveOutcome> {
        let gsid = *self.addr_index.lock().unwrap().get(peer)?;
        let mut matches = self.matches.lock().unwrap();
        let m = matches.get_mut(&gsid)?;
        let sender = m.players.iter().position(|p| &p.addr == peer)?;

        // Each command resets the ChaCha20 counter to 0 — encrypt and decrypt are
        // the same XOR against a fresh keystream (spec §4). Decrypt with sender key.
        let mut plain = user_data.to_vec();
        {
            let c = &m.players[sender].crypto;
            chacha20_legacy_xor(&mut plain, &c.key, &c.nonce);
        }
        let marker = plain.first().copied();
        let opcode = plain.get(1).copied(); // user_data[1] = GameMessageId

        let now = Instant::now();
        let mut replies: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        for (target, mut user_data) in m.instance.on_c2s(sender, &plain, now) {
            let Some(tp) = m.players.get(target) else {
                continue; // target not connected yet — drop (UDP-correct)
            };
            // `user_data` is the full decrypted s2c payload (marker ‖ type ‖ body)
            // from the engine; encrypt under the TARGET's key — this is where the
            // A→B relay happens.
            chacha20_legacy_xor(&mut user_data, &tp.crypto.key, &tp.crypto.nonce);
            replies.push((tp.addr, user_data));
        }
        Some(LiveOutcome {
            opcode,
            marker,
            replies,
            state: m.instance.state_name(),
        })
    }

    pub fn is_active(&self, peer: &SocketAddr) -> bool {
        self.addr_index.lock().unwrap().contains_key(peer)
    }

    /// Drop a peer from its match (disconnect). When the last player leaves, the
    /// match is removed and its capacity permit released.
    pub fn remove(&self, peer: &SocketAddr) {
        let Some(gsid) = self.addr_index.lock().unwrap().remove(peer) else {
            return;
        };
        let mut matches = self.matches.lock().unwrap();
        if let Some(m) = matches.get_mut(&gsid) {
            m.players.retain(|p| &p.addr != peer);
            if m.players.is_empty() {
                matches.remove(&gsid); // drops the permit → frees a Semaphore slot
                info!("match registry: match {gsid} empty, removed");
            } else {
                info!(
                    "match registry: {peer} left match {gsid} [{}/{}]",
                    m.players.len(),
                    m.capacity
                );
            }
        }
    }

    pub fn active_count(&self) -> usize {
        self.matches.lock().unwrap().len()
    }
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Reclaim leaked/abandoned matches + their capacity permits — called
    /// periodically by the ENet serve loop. The matchmaker acquires a permit in
    /// `allocate`, but it is otherwise only released when the LAST player
    /// disconnects (`remove`); a paired match whose clients never ENet-connect
    /// would hold it forever. Reclaim when under-capacity past `CONNECT_DEADLINE`
    /// (an opponent never connected — the abandoned-`Succeeded` leak) or older than
    /// `MATCH_MAX_AGE` (safety net for a stuck full match). Dropping the `Match`
    /// frees its `Semaphore` slot. Collect-then-purge so the locks never nest.
    pub fn sweep_expired(&self, now: Instant) {
        let mut reclaimed: Vec<(Uuid, usize, usize, &'static str, Vec<SocketAddr>)> = Vec::new();
        {
            let mut matches = self.matches.lock().unwrap();
            let dead: Vec<Uuid> = matches
                .values()
                .filter(|m| {
                    let age = now.saturating_duration_since(m.created_at);
                    (m.players.len() < m.capacity && age > CONNECT_DEADLINE) || age > MATCH_MAX_AGE
                })
                .map(|m| m.game_session_id)
                .collect();
            for gsid in dead {
                if let Some(m) = matches.remove(&gsid) {
                    let reason = if m.players.len() < m.capacity {
                        "opponent never connected"
                    } else {
                        "max age"
                    };
                    let addrs: Vec<SocketAddr> = m.players.iter().map(|p| p.addr).collect();
                    reclaimed.push((gsid, m.players.len(), m.capacity, reason, addrs));
                    // `m` (with its `_permit`) drops here → a Semaphore slot is freed.
                }
            }
        }
        if reclaimed.is_empty() {
            return;
        }
        let dead_gsids: std::collections::HashSet<Uuid> =
            reclaimed.iter().map(|(g, ..)| *g).collect();
        {
            let mut addr_index = self.addr_index.lock().unwrap();
            for (_, _, _, _, addrs) in &reclaimed {
                for a in addrs {
                    addr_index.remove(a);
                }
            }
        }
        self.pending
            .lock()
            .unwrap()
            .retain(|_, gsid| !dead_gsids.contains(gsid));
        for (gsid, connected, capacity, reason, _) in &reclaimed {
            warn!(
                "match registry: reclaimed match {gsid} ({reason}; {connected}/{capacity} connected) — {} slot(s) free of {}",
                self.semaphore.available_permits(),
                self.max_matches
            );
        }
    }

    /// Drive the per-match tick: server-initiated s2c (the flow-control state
    /// machine, plus DoT/cooldown/round logic in Phase C). Called once per ENet
    /// service-loop iteration. Returns `(target peer addr, encrypted user-data)`
    /// to send. Same lock discipline as `handle_live_user_data` — short,
    /// synchronous, never held across `.await`.
    pub fn tick_matches(&self, now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut matches = self.matches.lock().unwrap();
        let mut out = Vec::new();
        for m in matches.values_mut() {
            let connected = m.players.len();
            for (target, mut user_data) in m.instance.on_tick(connected, now) {
                let Some(tp) = m.players.get(target) else {
                    continue;
                };
                chacha20_legacy_xor(&mut user_data, &tp.crypto.key, &tp.crypto.nonce);
                out.push((tp.addr, user_data));
            }
        }
        out
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

/// What a decoded inbound datagram produced on the raw path: the decoded opcode
/// (for logging), the s2c reply datagrams to send to the addressed peer (already
/// ENet-framed + encrypted), and the match's resulting state name.
pub struct InboundOutcome {
    pub opcode: Option<u8>,
    pub replies: Vec<Vec<u8>>,
    pub state: &'static str,
}

/// What a decoded **live-host** (rusty_enet) SEND payload produced. Each reply is
/// `(target peer addr, encrypted user-data)` — the target may be the sender or the
/// opponent (relay); the ENet framing is rusty_enet's job, not ours.
pub struct LiveOutcome {
    pub opcode: Option<u8>,
    /// The decrypted marker byte (`0x84` c2s / `0xBE` s2c / `0xAC`); a value
    /// outside that set usually means a wrong key (handshake mismatch).
    pub marker: Option<u8>,
    pub replies: Vec<(SocketAddr, Vec<u8>)>,
    pub state: &'static str,
}

// The per-match state machine now lives in `crate::arena::combat::engine`
// (`MatchInstance`), driven by the real captured protocol (the flow-control
// stateName machine + authoritative combat). The placeholder FSM that used to
// live here — `PlayerLoadoutReady → PlayerWelcome + PlayerSpawnAvatar`,
// `PlayerCommand → PlayerStateChange`, `ConcedeMatch → MatchEndMatchMsg` — was
// removed: those opcodes never appear in real captures (see the combat module).
