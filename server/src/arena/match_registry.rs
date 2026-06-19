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
use crate::arena::key_submit::KeySubmitter;

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
    /// When set, every admitted peer's per-match (key, nonce) is fire-and-forget
    /// POSTed to the capture platform so OUR-server matches become decryptable
    /// (see [`key_submit`](crate::arena::key_submit)). `None` in tests / when
    /// submission is disabled — then admit is unchanged.
    key_submitter: Option<Arc<KeySubmitter>>,
    /// **DEBUG/experimental** packet-injection queue (see
    /// [`crate::arena::debug_inject`]). Actix debug routes push raw decrypted s2c
    /// `user_data` here; the ENet serve loop drains it each tick via
    /// [`drain_debug_injections`](Self::drain_debug_injections), encrypting each
    /// under the TARGET peer's `CryptoCtx`. Empty + untouched in normal operation.
    debug_inject_queue: Mutex<Vec<DebugInjection>>,
    /// **DEBUG (`ARENA_DEBUG_HOLD`).** When set, `sweep_expired` will NOT reclaim a
    /// match for being under-capacity (a solo peer with no opponent) or for max-age
    /// — a single connected peer persists indefinitely so we can hold it at the
    /// round-start and hand-inject s2c frames. A real ENet disconnect still removes
    /// the peer (`remove`). OFF (false) in all normal operation + tests → the sweep
    /// is unchanged.
    debug_hold: bool,
}

/// **DEBUG/experimental.** One queued packet injection: raw decrypted s2c
/// `user_data` (`0xBE ‖ MessageType ‖ body`) to encrypt under the target peer(s)'
/// key and send. `target` selects which connected peer(s) in the match receive it.
pub struct DebugInjection {
    pub gsid: Uuid,
    pub target: DebugTarget,
    pub plaintext: Vec<u8>,
}

/// **DEBUG.** Which connected peer(s) of a match an injection targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugTarget {
    /// A single slot (0 = first-admitted peer, 1 = second).
    Slot(usize),
    /// Every connected peer in the match.
    Both,
}

/// **DEBUG.** A snapshot of one live match + its peers, for the
/// `/arena/debug/peers` listing. Read-only; built under the registry lock.
pub struct DebugMatchView {
    pub game_session_id: Uuid,
    pub order: u64,
    pub capacity: usize,
    pub phase: &'static str,
    pub peers: Vec<DebugPeerView>,
}

/// **DEBUG.** One connected peer within a [`DebugMatchView`].
pub struct DebugPeerView {
    pub slot: usize,
    pub addr: SocketAddr,
    pub player_session_id: String,
    /// Character display name from the fighter's loadout (empty if unknown).
    pub character_name: String,
    /// Hex of the 8-byte ChaCha20 nonce this peer's s2c stream uses. The cipher
    /// resets the counter to 0 **per command** (spec §4) — there is no stateful
    /// send-nonce counter; every frame (both directions) is encrypted under this
    /// fixed (key, nonce) at counter 0, so an injected frame can never desync the
    /// stream. Exposed here as the per-peer crypto identity, not a running counter.
    pub nonce_hex: String,
}

/// **DEBUG.** What one injected frame produced: the peer it was sent to and the
/// ciphertext length (== plaintext length — XOR preserves length).
pub struct DebugInjectResult {
    pub slot: usize,
    pub addr: SocketAddr,
    pub nonce_hex: String,
    pub ciphertext_len: usize,
}

impl MatchRegistry {
    /// Test/dev constructor: no key submission.
    pub fn new(max_matches: usize) -> Arc<Self> {
        Self::new_with_submitter(max_matches, None)
    }

    /// Test-only: build a registry with the DEBUG-HOLD sweep-disable flag forced
    /// (the process env is never mutated by tests). Mirrors `ARENA_DEBUG_HOLD`.
    #[cfg(test)]
    pub fn new_with_debug_hold(max_matches: usize, hold: bool) -> Arc<Self> {
        Arc::new(MatchRegistry {
            semaphore: Arc::new(Semaphore::new(max_matches)),
            pending: Mutex::new(HashMap::new()),
            matches: Mutex::new(HashMap::new()),
            addr_index: Mutex::new(HashMap::new()),
            next_order: std::sync::atomic::AtomicU64::new(0),
            max_matches,
            key_submitter: None,
            debug_inject_queue: Mutex::new(Vec::new()),
            debug_hold: hold,
        })
    }

    /// Production constructor: `key_submitter` (if `Some`) receives every
    /// admitted peer's per-match key for submission to the capture platform.
    pub fn new_with_submitter(
        max_matches: usize,
        key_submitter: Option<Arc<KeySubmitter>>,
    ) -> Arc<Self> {
        Arc::new(MatchRegistry {
            semaphore: Arc::new(Semaphore::new(max_matches)),
            pending: Mutex::new(HashMap::new()),
            matches: Mutex::new(HashMap::new()),
            addr_index: Mutex::new(HashMap::new()),
            next_order: std::sync::atomic::AtomicU64::new(0),
            max_matches,
            key_submitter,
            debug_inject_queue: Mutex::new(Vec::new()),
            // Read the DEBUG-HOLD freeze flag once at startup (off when unset → all
            // tests + normal operation). Same parse as the MatchInstance flag.
            debug_hold: crate::arena::combat::debug_hold_enabled(),
        })
    }

    /// Fire-and-forget submit of an admitted peer's key+nonce (no-op when the
    /// submitter is absent). Called from both admit paths.
    fn submit_key(&self, crypto: &CryptoCtx) {
        if let Some(s) = &self.key_submitter {
            s.submit(&crypto.key, &crypto.nonce);
        }
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
        let mut instance = MatchInstance::new(fighters, capacity, loadouts, Instant::now());
        // The Match net-object propId9 = gameSessionId (s506 obj 123 carried the
        // match's UUID here). Cosmetic to the binding gate (propId5 MatchState), but
        // sent for fidelity.
        instance.set_game_session_id(game_session_id.to_string());
        self.matches.lock().unwrap().insert(
            game_session_id,
            Match {
                game_session_id,
                order,
                capacity,
                players: Vec::with_capacity(capacity),
                instance,
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
        let crypto = CryptoCtx { key, nonce };
        // Submit this peer's key to the capture platform (fire-and-forget; no-op
        // when disabled) so the match's captured frames become decryptable.
        self.submit_key(&crypto);
        m.players.push(PlayerConn {
            addr: peer,
            player_session_id: player_session_id.to_string(),
            crypto,
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
        let crypto = CryptoCtx { key, nonce };
        // Submit this peer's key to the capture platform (fire-and-forget; no-op
        // when disabled) so the match's captured frames become decryptable.
        self.submit_key(&crypto);
        m.players.push(PlayerConn {
            addr: peer,
            player_session_id: String::new(), // bound later if/when the psid arrives
            crypto,
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
        let mut replies: Vec<(SocketAddr, u8, Vec<u8>)> = Vec::new();
        for (target, mut user_data) in m.instance.on_c2s(sender, &plain, now) {
            let Some(tp) = m.players.get(target) else {
                continue; // target not connected yet — drop (UDP-correct)
            };
            // `user_data` is the full decrypted s2c payload (marker ‖ type ‖ body)
            // from the engine. Pick the retail ENet channel from the PLAINTEXT (by
            // carrier + GameMessageId — s506 map) BEFORE encrypting, then encrypt
            // under the TARGET's key (this is where the A→B relay happens).
            let channel = crate::arena::combat::messages::retail_channel(&user_data);
            chacha20_legacy_xor(&mut user_data, &tp.crypto.key, &tp.crypto.nonce);
            replies.push((tp.addr, channel, user_data));
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
        // DEBUG-HOLD (`ARENA_DEBUG_HOLD`): never reclaim a match for being
        // under-capacity (a solo peer with no opponent) or for max-age — a single
        // connected peer must persist indefinitely so we can hold it at the
        // round-start and hand-inject s2c frames. A real ENet disconnect still
        // removes the peer via `remove`; only the idle/capacity sweep is disabled.
        if self.debug_hold {
            return;
        }
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
    pub fn tick_matches(&self, now: Instant) -> Vec<(SocketAddr, u8, Vec<u8>)> {
        let mut matches = self.matches.lock().unwrap();
        let mut out = Vec::new();
        for m in matches.values_mut() {
            let connected = m.players.len();
            for (target, mut user_data) in m.instance.on_tick(connected, now) {
                let Some(tp) = m.players.get(target) else {
                    continue;
                };
                // Retail ENet channel from the PLAINTEXT (carrier + GMID, s506 map)
                // before encrypting under the TARGET's key.
                let channel = crate::arena::combat::messages::retail_channel(&user_data);
                chacha20_legacy_xor(&mut user_data, &tp.crypto.key, &tp.crypto.nonce);
                out.push((tp.addr, channel, user_data));
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // DEBUG / experimental packet-injection harness (crate::arena::debug_inject).
    // Token-gated actix routes use these to list live peers and to fire
    // hand-crafted, correctly-encrypted s2c frames into a LIVE peer so we can
    // observe which packet advances the stuck client. Inert in normal operation
    // (the queue is empty). Disable by removing the debug routes / not setting
    // ARENA_DEBUG_TOKEN. See docs in `debug_inject.rs`.
    // -----------------------------------------------------------------------

    /// **DEBUG.** Snapshot every live match + its connected peers (addr, slot,
    /// character name, the per-peer s2c nonce). Read-only; one short lock.
    pub fn debug_list(&self) -> Vec<DebugMatchView> {
        let matches = self.matches.lock().unwrap();
        let mut out: Vec<DebugMatchView> = matches
            .values()
            .map(|m| DebugMatchView {
                game_session_id: m.game_session_id,
                order: m.order,
                capacity: m.capacity,
                phase: m.instance.state_name(),
                peers: m
                    .players
                    .iter()
                    .enumerate()
                    .map(|(slot, p)| DebugPeerView {
                        slot,
                        addr: p.addr,
                        player_session_id: p.player_session_id.clone(),
                        character_name: m.instance.fighter_display_name(slot).to_string(),
                        nonce_hex: hex_lower(&p.crypto.nonce),
                    })
                    .collect(),
            })
            .collect();
        out.sort_by_key(|m| m.order); // stable display order (allocation order)
        out
    }

    /// **DEBUG.** Resolve a match by `gameSessionId` (exact). Returns `None` if no
    /// such live match. Used by the inject route to validate the target up-front.
    pub fn debug_match_exists(&self, gsid: &Uuid) -> bool {
        self.matches.lock().unwrap().contains_key(gsid)
    }

    /// **DEBUG.** Queue a raw decrypted s2c `user_data` for injection into a live
    /// match's peer(s) on the next ENet tick. Returns how many connected peers the
    /// target currently resolves to (so the caller can 404 an empty match), without
    /// sending yet — the ENet loop owns the encrypt+send (see
    /// [`drain_debug_injections`](Self::drain_debug_injections)). `None` ⇒ no such match.
    pub fn debug_enqueue(&self, gsid: Uuid, target: DebugTarget, plaintext: Vec<u8>) -> Option<usize> {
        let resolved = {
            let matches = self.matches.lock().unwrap();
            let m = matches.get(&gsid)?;
            match target {
                DebugTarget::Slot(s) => usize::from(s < m.players.len()),
                DebugTarget::Both => m.players.len(),
            }
        };
        self.debug_inject_queue.lock().unwrap().push(DebugInjection {
            gsid,
            target,
            plaintext,
        });
        Some(resolved)
    }

    /// **DEBUG.** Drain the injection queue, encrypting each queued frame under the
    /// TARGET peer's `CryptoCtx` — the SAME encrypt path as `tick_matches` /
    /// `handle_live_user_data` (ChaCha20, counter 0, the peer's fixed nonce). Returns
    /// `(target peer addr, encrypted user-data)` for the ENet loop to send (routed
    /// by length, like the normal paths), plus a per-frame [`DebugInjectResult`] for
    /// the log line. Called once per ENet serve-loop iteration; a no-op (no lock
    /// contention beyond an empty-vec check) when nothing is queued.
    pub fn drain_debug_injections(&self) -> Vec<(SocketAddr, u8, Vec<u8>, DebugInjectResult)> {
        let queued: Vec<DebugInjection> = {
            let mut q = self.debug_inject_queue.lock().unwrap();
            if q.is_empty() {
                return Vec::new();
            }
            std::mem::take(&mut *q)
        };
        let mut out = Vec::new();
        let matches = self.matches.lock().unwrap();
        for inj in queued {
            let Some(m) = matches.get(&inj.gsid) else {
                warn!(
                    "arena DEBUG inject: match {} gone before send — dropping {} B frame",
                    inj.gsid,
                    inj.plaintext.len()
                );
                continue;
            };
            let slots: Vec<usize> = match inj.target {
                DebugTarget::Slot(s) if s < m.players.len() => vec![s],
                DebugTarget::Slot(_) => Vec::new(),
                DebugTarget::Both => (0..m.players.len()).collect(),
            };
            // Retail ENet channel from the injected PLAINTEXT (carrier + GMID).
            let channel = crate::arena::combat::messages::retail_channel(&inj.plaintext);
            for slot in slots {
                let p = &m.players[slot];
                let mut ct = inj.plaintext.clone();
                chacha20_legacy_xor(&mut ct, &p.crypto.key, &p.crypto.nonce);
                let result = DebugInjectResult {
                    slot,
                    addr: p.addr,
                    nonce_hex: hex_lower(&p.crypto.nonce),
                    ciphertext_len: ct.len(),
                };
                out.push((p.addr, channel, ct, result));
            }
        }
        out
    }
}

/// Lowercase-hex a byte slice (DEBUG peer/nonce display; no extra dep).
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
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
    /// `(target peer addr, retail ENet channel, encrypted user-data)`.
    pub replies: Vec<(SocketAddr, u8, Vec<u8>)>,
    pub state: &'static str,
}

// The per-match state machine now lives in `crate::arena::combat::engine`
// (`MatchInstance`), driven by the real captured protocol (the flow-control
// stateName machine + authoritative combat). The placeholder FSM that used to
// live here — `PlayerLoadoutReady → PlayerWelcome + PlayerSpawnAvatar`,
// `PlayerCommand → PlayerStateChange`, `ConcedeMatch → MatchEndMatchMsg` — was
// removed: those opcodes never appear in real captures (see the combat module).
