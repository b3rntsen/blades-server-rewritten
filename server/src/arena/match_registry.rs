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

use log::{error, info, warn};
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

/// How long a full match waits for every peer's fighter slot to become
/// **authoritatively** known before it gives up and proceeds on the FIFO admission
/// order (loudly — see [`Match::slot_binding_ready`]).
///
/// Why a wait is needed at all: the round-start identity burst (spawns → avatars →
/// opponent profile) is built per viewer and addressed by FIGHTER SLOT; the registry
/// turns that slot into a peer. On the live ENet path the op-0x38 handshake carries
/// no `playerSessionId` (only a 6-byte conn id + the X25519 pubkey), so the slot is
/// unknown until the client's first identity-bearing message is decrypted. Emitting
/// the burst before then addresses it by admission order, and when admission order
/// differs from ticket order each client is handed the OTHER player's identity — the
/// "the opponent looks like me" bug.
///
/// Why 3 s is enough: the client uploads `PlayerInfo`/`PlayerLoadoutReady` within a
/// few hundred ms of the handshake, and retail's own round-start has ~2 s of slack
/// between `BackendMatchCreated` and the live round on top of the 1 s
/// `MATCH_SETUP_STAGGER` we already spend between the spawns and the avatars.
///
/// Why it is BOUNDED: a client that never sends an identity-bearing message must not
/// be able to wedge a match open forever. Past this window we commit the FIFO order —
/// possibly wrong, but deterministic and self-consistent — and log an `error!`.
const SLOT_BIND_GRACE: Duration = Duration::from_secs(3);

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
    /// `playerSessionId → fighter slot` — populated at allocation so that when the
    /// client's encrypted PlayerInfo (op20) arrives, we can bind its peer address to
    /// the correct fighter slot (rather than the FIFO admission order, which may not
    /// match ticket order — see arena-multiplayer Bug 4 / slot inversion).
    /// Example: psids[0] = Flappety's psid → slot 0, psids[1] = WolfWalker's psid → slot 1.
    psid_to_slot: HashMap<String, usize>,
    /// `peer address → fighter slot` — the **authoritative** addressing table, in both
    /// directions (inbound sender resolution and outbound target resolution). Populated
    /// as early as each path allows:
    ///   - [`admit`](MatchRegistry::admit) — immediately, from the presented psid;
    ///   - [`admit_connection`](MatchRegistry::admit_connection) — immediately when the
    ///     match has a single peer slot (only one possible assignment), otherwise by
    ///     elimination or from the first identity-bearing c2s frame
    ///     (`PlayerInfo`/`PlayerLoadoutReady`) in `handle_live_user_data`;
    ///   - as a bounded last resort, from the FIFO admission order after
    ///     [`SLOT_BIND_GRACE`] (logged as an error — see `slot_binding_ready`).
    ///
    /// Nothing that carries player IDENTITY may be addressed before this is complete;
    /// `tick_matches` holds the round-start burst until it is.
    peer_to_slot: HashMap<SocketAddr, usize>,
    /// First tick at which this match was full (`players.len() == capacity`) — the
    /// start of the [`SLOT_BIND_GRACE`] window. Set from the TICK clock (not
    /// wall-clock) so the deadline is deterministic under a virtual clock.
    full_at: Option<Instant>,
    /// Log-once latch: the grace window expired and we committed the FIFO order.
    fallback_logged: bool,
    /// Log-once latch: a frame had to be addressed with no authoritative binding.
    misaddress_logged: bool,
}

impl Match {
    /// True once every connected peer has an authoritative fighter slot (and the
    /// match is actually full — a half-connected match is the connect gate's problem,
    /// not this one's).
    fn all_slots_bound(&self) -> bool {
        self.players.len() >= self.capacity
            && self
                .players
                .iter()
                .all(|p| self.peer_to_slot.contains_key(&p.addr))
    }

    /// Bind `peer` to fighter `slot`. Idempotent. A **conflicting** re-bind is refused
    /// and logged as an error: once frames have gone out under a mapping, silently
    /// swapping a peer's identity mid-match is strictly worse than the original
    /// mistake. `how` names the evidence, for the log.
    fn bind_slot(&mut self, peer: SocketAddr, slot: usize, how: &str) -> bool {
        match self.peer_to_slot.get(&peer) {
            Some(&existing) if existing == slot => return false, // already known
            Some(&existing) => {
                error!(
                    "match registry: match {} — peer {peer} is bound to fighter slot {existing} \
                     but {how} says slot {slot}. REFUSING the re-bind (frames already sent used \
                     the old mapping). Identity addressing for this match is UNTRUSTWORTHY.",
                    self.game_session_id
                );
                return false;
            }
            None => {}
        }
        if let Some((holder, _)) = self.peer_to_slot.iter().find(|&(_, &s)| s == slot) {
            error!(
                "match registry: match {} — fighter slot {slot} is already held by peer {holder}; \
                 refusing to also bind {peer} ({how}).",
                self.game_session_id
            );
            return false;
        }
        self.peer_to_slot.insert(peer, slot);
        info!(
            "match registry: match {} — peer {peer} → fighter slot {slot} ({how}) [{}/{} bound]",
            self.game_session_id,
            self.peer_to_slot.len(),
            self.capacity,
        );
        self.bind_by_elimination();
        true
    }

    /// N peers, N fighter slots: once N−1 are bound the last one is determined. This
    /// is what lets a 2-player live-ENet match bind BOTH peers off a single identity
    /// message (or off the one peer whose psid we happened to learn).
    fn bind_by_elimination(&mut self) {
        if self.players.len() < self.capacity {
            return; // an unadmitted peer could still claim the free slot
        }
        let unbound: Vec<SocketAddr> = self
            .players
            .iter()
            .map(|p| p.addr)
            .filter(|a| !self.peer_to_slot.contains_key(a))
            .collect();
        if unbound.len() != 1 {
            return;
        }
        let free: Vec<usize> = (0..self.capacity)
            .filter(|s| !self.peer_to_slot.values().any(|&b| b == *s))
            .collect();
        if free.len() != 1 {
            return;
        }
        self.peer_to_slot.insert(unbound[0], free[0]);
        info!(
            "match registry: match {} — peer {} → fighter slot {} (by elimination: last free slot) \
             [{}/{} bound]",
            self.game_session_id,
            unbound[0],
            free[0],
            self.peer_to_slot.len(),
            self.capacity,
        );
    }

    /// May the round-start identity burst be emitted yet?
    ///
    /// `true` when every peer's fighter slot is authoritative — or when the bounded
    /// [`SLOT_BIND_GRACE`] has expired, in which case we commit the FIFO admission
    /// order (so the match proceeds rather than hanging) and shout about it.
    fn slot_binding_ready(&mut self, now: Instant) -> bool {
        // A single-peer match (solo / vs-bot) has exactly one possible assignment, so
        // admission order cannot be wrong. Nothing to wait for.
        if self.capacity <= 1 || self.all_slots_bound() {
            return true;
        }
        if self.players.len() < self.capacity {
            // Not everyone has even connected — the engine's own
            // `connected >= expected_peers` gate owns this case; don't start the clock.
            return false;
        }
        let full_at = *self.full_at.get_or_insert(now);
        if now.saturating_duration_since(full_at) < SLOT_BIND_GRACE {
            return false;
        }
        if !self.fallback_logged {
            self.fallback_logged = true;
            let unbound: Vec<SocketAddr> = self
                .players
                .iter()
                .map(|p| p.addr)
                .filter(|a| !self.peer_to_slot.contains_key(a))
                .collect();
            error!(
                "match registry: match {} — no identity-bearing frame (PlayerInfo/PlayerLoadoutReady) \
                 from {unbound:?} within {:?} of the match filling up. Proceeding on the FIFO \
                 ADMISSION ORDER so the match cannot hang — but if admission order ≠ ticket order \
                 these players' identities are SWAPPED (each sees the opponent wearing its own \
                 appearance). psid_to_slot = {:?}, bound so far = {:?}",
                self.game_session_id,
                SLOT_BIND_GRACE,
                self.psid_to_slot,
                self.peer_to_slot,
            );
        }
        self.commit_fifo_binding();
        true
    }

    /// Last-resort: write the FIFO admission order into `peer_to_slot`, preserving any
    /// binding already established. Making it explicit (rather than leaving peers
    /// unbound and re-deriving FIFO per frame) keeps the whole match self-consistent
    /// and makes a later contradicting `PlayerInfo` a loud conflict instead of a
    /// silent mid-match identity swap.
    fn commit_fifo_binding(&mut self) {
        let addrs: Vec<SocketAddr> = self.players.iter().map(|p| p.addr).collect();
        let mut taken: std::collections::HashSet<usize> =
            self.peer_to_slot.values().copied().collect();
        for (i, addr) in addrs.into_iter().enumerate() {
            if self.peer_to_slot.contains_key(&addr) {
                continue;
            }
            let slot = if i < self.capacity && !taken.contains(&i) {
                i
            } else {
                match (0..self.capacity).find(|s| !taken.contains(s)) {
                    Some(s) => s,
                    None => continue,
                }
            };
            taken.insert(slot);
            self.peer_to_slot.insert(addr, slot);
        }
    }

    /// Resolve an engine target FIGHTER SLOT to a connected peer address.
    ///
    /// The authoritative `peer_to_slot` map is the only correct answer. `None` for a
    /// slot with no peer is normal and silent (a bot fighter, or an opponent who has
    /// not connected — UDP-correct to drop). Falling back to the FIFO index for a slot
    /// that *does* have a peer is a **correctness violation**, so it is logged as an
    /// error (once per match, since it would otherwise repeat every frame).
    fn peer_for_slot(&mut self, slot: usize) -> Option<SocketAddr> {
        if let Some((addr, _)) = self.peer_to_slot.iter().find(|&(_, &s)| s == slot) {
            return Some(*addr);
        }
        let fallback = self.players.get(slot)?.addr;
        if !self.misaddress_logged {
            self.misaddress_logged = true;
            error!(
                "match registry: match {} — fighter slot {slot} has NO authoritative peer binding; \
                 addressing it by FIFO admission index → {fallback}. If admission order ≠ ticket \
                 order this frame is going to the WRONG player. peer_to_slot = {:?}, FIFO = {:?}",
                self.game_session_id,
                self.peer_to_slot,
                self.players.iter().map(|p| p.addr).collect::<Vec<_>>(),
            );
        }
        Some(fallback)
    }
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
        // psid → fighter slot: psids[i] was allocated for ticket[i] which becomes
        // fighter slot i. Store this so handle_live_user_data can remap the peer's
        // ENet slot (connection order) to the correct fighter slot once the psid
        // is extracted from the client's first encrypted PlayerInfo (op20) message.
        let psid_to_slot: HashMap<String, usize> = player_session_ids
            .iter()
            .enumerate()
            .map(|(i, psid)| (psid.clone(), i))
            .collect();
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
                psid_to_slot,
                peer_to_slot: HashMap::new(),
                full_at: None,
                fallback_logged: false,
                misaddress_logged: false,
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
        // The ticket order is the ONLY authority on who is which fighter, and on this
        // path the client just presented the psid that names it. Bind NOW — not lazily
        // from a later PlayerInfo — so nothing (least of all the round-start identity
        // burst, which fires before any op20) is ever addressed by admission order.
        let ticket_slot = m.psid_to_slot.get(player_session_id).copied();
        m.players.push(PlayerConn {
            addr: peer,
            player_session_id: player_session_id.to_string(),
            crypto,
        });
        match ticket_slot {
            Some(slot) => {
                m.bind_slot(peer, slot, "playerSessionId presented at admit");
            }
            // Shouldn't happen: `pending` and `psid_to_slot` are written together in
            // `allocate`. If it ever does, say so — the peer will be addressed by FIFO.
            None => error!(
                "match registry: match {gsid} — admitted psess {player_session_id} is absent from \
                 psid_to_slot ({:?}); this peer has NO authoritative fighter slot.",
                m.psid_to_slot
            ),
        }
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
        // No psid on the wire — but two cases are still decidable right here:
        //   1. a single-peer match (solo / vs-bot) has exactly ONE possible fighter
        //      slot, so admission order cannot be wrong;
        //   2. if every other peer is already bound, this one is the last free slot.
        // Everything else waits for an identity-bearing c2s frame (or the bounded
        // SLOT_BIND_GRACE fallback); `tick_matches` holds the identity burst until then.
        if m.capacity <= 1 {
            let slot = m.psid_to_slot.values().copied().min().unwrap_or(0);
            m.bind_slot(peer, slot, "single-peer match — only one possible fighter slot");
        } else {
            m.bind_by_elimination();
        }
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
        // ENet-admission index (FIFO connection order) — used to look up this peer's
        // crypto key. May NOT equal the fighter slot if connection order ≠ ticket order.
        let enet_slot = m.players.iter().position(|p| &p.addr == peer)?;

        // Each command resets the ChaCha20 counter to 0 — encrypt and decrypt are
        // the same XOR against a fresh keystream (spec §4). Decrypt with sender key.
        let mut plain = user_data.to_vec();
        {
            let c = &m.players[enet_slot].crypto;
            chacha20_legacy_xor(&mut plain, &c.key, &c.nonce);
        }
        let marker = plain.first().copied();
        let opcode = plain.get(1).copied(); // user_data[1] = GameMessageId

        // Fighter-slot resolution: if this peer isn't authoritatively bound yet, try to
        // extract its playerSessionId from an identity-bearing c2s frame (`PlayerInfo`
        // or `PlayerLoadoutReady`) and resolve it through `psid_to_slot` (built at
        // allocation: psids[i] → fighter slot i). On the live ENet path this is the
        // FIRST moment the psid exists on the wire at all — the op-0x38 handshake
        // carries none — so it is also what releases the held identity burst.
        if !m.peer_to_slot.contains_key(peer) {
            if let Some(slot) = extract_fighter_slot_from_playerinfo(&plain, &m.psid_to_slot) {
                m.bind_slot(*peer, slot, "playerSessionId in an identity-bearing c2s frame");
            }
        }
        // Use the authoritative fighter slot if available, else the FIFO index. (An
        // unbound sender only mis-resolves its OWN inputs, which is harmless before the
        // round is live; the identity burst — the part that matters — is held by
        // `tick_matches` until the binding exists.)
        let sender = m.peer_to_slot.get(peer).copied().unwrap_or(enet_slot);

        let now = Instant::now();
        let mut replies: Vec<(SocketAddr, u8, Vec<u8>)> = Vec::new();
        for (target, mut user_data) in m.instance.on_c2s(sender, &plain, now) {
            // Resolve the engine's target FIGHTER SLOT to a connected PEER (authoritative
            // map; loud FIFO fallback — see `Match::peer_for_slot`).
            let Some(target_addr) = m.peer_for_slot(target) else {
                continue; // target not connected yet (or a bot) — drop (UDP-correct)
            };
            let Some(tp) = m.players.iter().find(|p| p.addr == target_addr) else {
                continue;
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
            // IDENTITY GATE. The engine fires its whole round-start burst — spawns,
            // avatars, the opponent op54 profile — on the single tick where
            // `connected >= expected_peers`, and every frame in it is addressed by
            // FIGHTER SLOT. If a peer's slot is still unknown at that moment the burst
            // is addressed by admission order instead, and when admission order ≠ ticket
            // order the two players' identities are swapped for the whole match (each
            // renders the opponent with its own appearance). So while the FSM is still
            // in `Connecting`, report ZERO connected peers until every slot is bound —
            // holding the burst, not the match: `slot_binding_ready` gives up after a
            // bounded `SLOT_BIND_GRACE` and proceeds on FIFO with an `error!`.
            let ready = !m.instance.is_connecting() || m.slot_binding_ready(now);
            let connected = if ready { m.players.len() } else { 0 };
            for (target, mut user_data) in m.instance.on_tick(connected, now) {
                // Resolve fighter slot → peer addr (authoritative map; loud FIFO
                // fallback — same as `handle_live_user_data`).
                let Some(target_addr) = m.peer_for_slot(target) else {
                    continue;
                };
                let Some(tp) = m.players.iter().find(|p| p.addr == target_addr) else {
                    continue;
                };
                // Retail ENet channel from the PLAINTEXT (carrier + GMID, s506 map)
                // before encrypting under the TARGET's key.
                let channel = crate::arena::combat::messages::retail_channel(&user_data);
                let key = tp.crypto.key;
                let nonce = tp.crypto.nonce;
                let addr = tp.addr;
                chacha20_legacy_xor(&mut user_data, &key, &nonce);
                out.push((addr, channel, user_data));
            }
        }
        out
    }

    /// Collect + RETIRE every match whose FSM has reached the terminal
    /// `Finished` state (the post-match MatchState walk completed at
    /// `DisconnectingPlayersAfterMatch`(19)). Returns the peer addresses of those
    /// matches so the ENet loop can actively DISCONNECT them — which is what the
    /// `DisconnectingPlayersAfterMatch` state literally is, and what makes the
    /// client leave the result screen and return to the arena lobby. The match is
    /// removed here (freeing its `Semaphore` permit + the addr-index entries), so a
    /// re-FIGHT gets a fresh allocation. Idempotent: a finished match is taken once.
    /// Same short-lock discipline as `tick_matches`.
    pub fn take_finished_peers(&self) -> Vec<SocketAddr> {
        let mut matches = self.matches.lock().unwrap();
        let finished: Vec<Uuid> = matches
            .values()
            .filter(|m| m.instance.is_finished())
            .map(|m| m.game_session_id)
            .collect();
        if finished.is_empty() {
            return Vec::new();
        }
        let mut addrs = Vec::new();
        for gsid in &finished {
            if let Some(m) = matches.remove(gsid) {
                for p in &m.players {
                    addrs.push(p.addr);
                }
                info!(
                    "match registry: match {gsid} Finished (post-match walk complete) — \
                     disconnecting {} peer(s), freeing the slot",
                    m.players.len(),
                );
                // `m` (+ its `_permit`) drops here → a Semaphore slot is freed.
            }
        }
        drop(matches);
        {
            let mut addr_index = self.addr_index.lock().unwrap();
            for a in &addrs {
                addr_index.remove(a);
            }
        }
        let dead: std::collections::HashSet<Uuid> = finished.into_iter().collect();
        self.pending.lock().unwrap().retain(|_, gsid| !dead.contains(gsid));
        addrs
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

/// Does this decrypted c2s frame plausibly carry the client's `playerSessionId`?
///
/// Two wire shapes are accepted, because binding EARLY is the whole point — the
/// identity burst is held until every peer is bound, so every frame we can legally
/// learn a psid from shortens that hold:
///   - **carrier byte == 20** — the flat dev/raw shape the existing tests use.
///   - **carrier `0x36` (`MSGTYPE_USERMESSAGE`, "op54") with GameMessageId 20 or 36**
///     — the retail shape. Byte 1 is the MessageType CARRIER, not the
///     `GameMessageId`; the GMID lives at NetData propId 3. `PlayerInfo`(20) is the
///     earliest identity-bearing c2s frame and `PlayerLoadoutReady`(36) the next, and
///     both are classified handshake (never combat) by
///     [`messages::is_noncombat_user_message`], so reading them here is free.
///
/// Deliberately NOT "scan every frame": a psid substring hit is unambiguous, but
/// restricting the scan to identity/handshake frames keeps a stray blob of client
/// data from ever being able to re-bind a player.
fn carries_player_session_id(plain: &[u8]) -> bool {
    if plain.get(1) == Some(&20) {
        return true;
    }
    matches!(
        crate::arena::combat::messages::user_message_gmid(plain),
        Some(20) | Some(36)
    )
}

/// Fighter-slot resolution from a decrypted identity-bearing c2s payload.
///
/// The client sends a GMID-20 PlayerInfo early in the session (carrier 0x84,
/// opcode byte = 20) that carries the playerSessionId as a string property.
/// By scanning the plaintext for a UTF-8 string that matches one of the keys in
/// `psid_to_slot` we can bind the peer's ENet admission slot → the correct
/// fighter slot (allocated by the matchmaker, which owns the psid→slot mapping).
///
/// The encoding used by Blades is a Pascal-style varint-length-prefixed UTF-8
/// string embedded in a serialised protobuf-like structure.  We do NOT have the
/// full schema, so instead we scan the payload for any substring that matches a
/// known psid exactly (they are 36-char UUIDs with dashes — unambiguous). If
/// found, return the fighter slot for that psid; otherwise None.
///
/// Safety: we hold the registry lock while this runs, so it is synchronous and
/// must be fast. The payload is at most a few hundred bytes — a linear scan is
/// perfectly acceptable.
fn extract_fighter_slot_from_playerinfo(
    plain: &[u8],
    psid_to_slot: &HashMap<String, usize>,
) -> Option<usize> {
    // Only attempt extraction on identity-bearing handshake frames (PlayerInfo /
    // PlayerLoadoutReady), in either the flat or the retail carrier-0x36 shape.
    if !carries_player_session_id(plain) {
        return None;
    }
    // psids are 36-char UUID strings ("xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx").
    // Scan the plaintext as a byte slice; look for each known psid as a substring.
    for (psid, &slot) in psid_to_slot {
        let needle = psid.as_bytes();
        if needle.len() > plain.len() {
            continue;
        }
        if plain.windows(needle.len()).any(|w| w == needle) {
            return Some(slot);
        }
    }
    None
}

// The per-match state machine now lives in `crate::arena::combat::engine`
// (`MatchInstance`), driven by the real captured protocol (the flow-control
// stateName machine + authoritative combat). The placeholder FSM that used to
// live here — `PlayerLoadoutReady → PlayerWelcome + PlayerSpawnAvatar`,
// `PlayerCommand → PlayerStateChange`, `ConcedeMatch → MatchEndMatchMsg` — was
// removed: those opcodes never appear in real captures (see the combat module).

#[cfg(test)]
mod tests {
    use super::*;

    fn psid_map(pairs: &[(&str, usize)]) -> HashMap<String, usize> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// A fake op20 PlayerInfo plaintext: marker 0x84, GMID 20, then some bytes
    /// that embed the psid as a UTF-8 substring (simulating the wire encoding).
    fn fake_op20(psid: &str, extra_prefix: &[u8]) -> Vec<u8> {
        let mut buf = vec![0x84u8, 20u8];
        buf.extend_from_slice(extra_prefix);
        buf.extend_from_slice(psid.as_bytes());
        buf.extend_from_slice(b"\x00\x00trailing bytes");
        buf
    }

    #[test]
    fn psid_in_op20_resolves_correct_slot() {
        let psid0 = "aaaaaaaa-0000-0000-0000-000000000000";
        let psid1 = "bbbbbbbb-1111-1111-1111-111111111111";
        let map = psid_map(&[(psid0, 0), (psid1, 1)]);

        let plain = fake_op20(psid1, b"\x01\x02\x03");
        let slot = extract_fighter_slot_from_playerinfo(&plain, &map);
        assert_eq!(slot, Some(1), "psid1 should map to fighter slot 1");
    }

    #[test]
    fn psid_in_op20_slot0_works() {
        let psid0 = "cccccccc-2222-2222-2222-222222222222";
        let psid1 = "dddddddd-3333-3333-3333-333333333333";
        let map = psid_map(&[(psid0, 0), (psid1, 1)]);

        let plain = fake_op20(psid0, &[]);
        let slot = extract_fighter_slot_from_playerinfo(&plain, &map);
        assert_eq!(slot, Some(0), "psid0 should map to fighter slot 0");
    }

    #[test]
    fn non_op20_returns_none() {
        let psid = "eeeeeeee-4444-4444-4444-444444444444";
        let map = psid_map(&[(psid, 0)]);

        // GMID = 22, not 20 — should NOT extract even if psid is present
        let mut plain = fake_op20(psid, &[]);
        plain[1] = 22;
        let slot = extract_fighter_slot_from_playerinfo(&plain, &map);
        assert_eq!(slot, None, "non-op20 messages should return None");
    }

    #[test]
    fn unknown_psid_returns_none() {
        let known_psid = "ffffffff-5555-5555-5555-555555555555";
        let map = psid_map(&[(known_psid, 0)]);

        // op20 but carries a DIFFERENT psid not in the map
        let other_psid = "00000000-9999-9999-9999-999999999999";
        let plain = fake_op20(other_psid, &[]);
        let slot = extract_fighter_slot_from_playerinfo(&plain, &map);
        assert_eq!(slot, None, "unknown psid should return None");
    }

    #[test]
    fn empty_payload_returns_none() {
        let map = psid_map(&[("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", 0)]);
        let slot = extract_fighter_slot_from_playerinfo(&[], &map);
        assert_eq!(slot, None);
    }

    // -----------------------------------------------------------------------
    // Phase 0.4 — ENet admission order must NOT decide the fighter slot.
    //
    // `allocate` owns the authoritative ticket order (`psids[i]` → fighter slot
    // `i`) and stores it in `psid_to_slot`. But the peer→slot binding is only
    // ever populated LATER, from the client's first decrypted op20 `PlayerInfo`
    // (`handle_live_user_data`). Everything before that — including the entire
    // round-start identity burst, which `engine.rs` fires the moment
    // `connected >= expected_peers` (i.e. right after the two op-0x38
    // handshakes) — is routed by `m.players.get(target)`, the FIFO ADMISSION
    // order. When ENet admission order ≠ ticket order the burst is delivered to
    // the wrong peers: each client gets the other's spawn set and the other's
    // op54 profile, and renders the opponent with its own character's appearance.
    //
    // This test drives `admit` — the psid-BEARING admission path — deliberately:
    // it is the strongest possible statement of the defect, because there the
    // registry is handed the disambiguating playerSessionId and STILL binds FIFO.
    // The live ENet path (`admit_connection`, enet_host.rs) has it worse: op-0x38
    // carries no psid at all, so a correct fix there must either defer the
    // identity burst until each peer's op20 PlayerInfo has landed, or get the
    // psid onto the connection some other way. Phase 2's problem — not this one's.
    // -----------------------------------------------------------------------

    const PSID_A: &str = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
    const PSID_B: &str = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
    const UUID_A: &str = "11111111-1111-4111-8111-111111111111";
    const UUID_B: &str = "22222222-2222-4222-8222-222222222222";

    /// A fighter with a distinct identity + a non-empty profile (so the round-start
    /// op54 PROFILE is actually broadcast and we can see WHERE it lands).
    fn ident_loadout(name: &str, uuid: &str) -> Loadout {
        let mut l = crate::arena::combat::loadout::starter();
        l.display_name = name.to_string();
        l.character_uuid = uuid.to_string();
        l.profile_equipped_json = r#"{"equippedItems":{}}"#.to_string();
        l.profile_character_json = format!(r#"{{"id":"{uuid}","name":"{name}"}}"#);
        l
    }

    #[test]
    fn admission_order_reversed_still_binds_correctly() {
        let reg = MatchRegistry::new(4);
        let gsid = Uuid::new_v4();
        // Ticket order: psid A → fighter slot 0 (Alpha), psid B → fighter slot 1 (Bravo).
        assert!(reg.allocate(
            &[PSID_A.to_string(), PSID_B.to_string()],
            vec![ident_loadout("Alpha", UUID_A), ident_loadout("Bravo", UUID_B)],
            gsid,
        ));

        let addr_a: SocketAddr = "127.0.0.1:41000".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:41001".parse().unwrap();
        // *** B connects FIRST *** — ENet admission order is the REVERSE of ticket
        // order. Nothing about the network guarantees they agree.
        reg.admit(addr_b, PSID_B, &[7u8; 32]).expect("B admitted");
        reg.admit(addr_a, PSID_A, &[9u8; 32]).expect("A admitted");

        // Both halves below are checked, then reported TOGETHER — the slot binding
        // and the frame routing are two faces of the same defect and we want the
        // whole picture in one run, not just whichever assert fires first.
        let mut failures: Vec<String> = Vec::new();

        // ---- (1) peer → fighter slot must follow the TICKET, not the handshake --
        {
            let matches = reg.matches.lock().unwrap();
            let m = matches.get(&gsid).expect("match still live");
            assert_eq!(m.psid_to_slot.get(PSID_A), Some(&0), "allocation invariant");
            assert_eq!(m.psid_to_slot.get(PSID_B), Some(&1), "allocation invariant");
            if m.peer_to_slot.get(&addr_a) != Some(&0) {
                failures.push(format!(
                    "(1) BINDING: peer A → fighter slot {:?}, want Some(0). A presented psid \
                     {PSID_A} at admit — the registry ALREADY knows it is fighter slot 0 \
                     (psid_to_slot) — yet peer_to_slot was never populated, so every frame \
                     before A's op20 PlayerInfo is routed by the FIFO admission index. \
                     peer_to_slot = {:?}, FIFO players = {:?}",
                    m.peer_to_slot.get(&addr_a),
                    m.peer_to_slot,
                    m.players.iter().map(|p| p.addr).collect::<Vec<_>>(),
                ));
            }
            if m.peer_to_slot.get(&addr_b) != Some(&1) {
                failures.push(format!(
                    "(1) BINDING: peer B → fighter slot {:?}, want Some(1). B connected FIRST, \
                     so the FIFO index makes it slot 0, but its ticket (psid {PSID_B}) is \
                     fighter slot 1.",
                    m.peer_to_slot.get(&addr_b),
                ));
            }
        }

        // ---- (2) end-to-end: the round-start identity burst must reach the right
        //          peers. Each viewer gets exactly ONE op54 PROFILE = its OPPONENT's,
        //          so peer A (slot 0) must receive Bravo's and peer B slot 0's Alpha's.
        //          This is the frame the client builds the opponent actor from.
        let crypto: Vec<(SocketAddr, [u8; 32], [u8; 8])> = {
            let matches = reg.matches.lock().unwrap();
            let m = matches.get(&gsid).unwrap();
            m.players.iter().map(|p| (p.addr, p.crypto.key, p.crypto.nonce)).collect()
        };
        let t0 = Instant::now();
        let mut per_peer_profiles: HashMap<SocketAddr, Vec<String>> = HashMap::new();
        for i in 0..=60u32 {
            for (addr, _channel, ct) in reg.tick_matches(t0 + Duration::from_millis(100) * i) {
                let (_, key, nonce) = crypto
                    .iter()
                    .find(|(a, _, _)| *a == addr)
                    .copied()
                    .expect("reply addressed to a peer of this match");
                let mut plain = ct.clone();
                chacha20_legacy_xor(&mut plain, &key, &nonce);
                // op54 PROFILE: marker 0xBE, carrier 0x36, NetData propId3 == 35;
                // propId5 carries the character JSON.
                if plain.len() > 2 && plain[0] == 0xBE && plain[1] == 0x36 {
                    let nd = arena_proto::parse_netdata(&plain[2..]);
                    if nd.int(3) == Some(35) {
                        if let Some(json) = nd.string(5) {
                            per_peer_profiles.entry(addr).or_default().push(json.to_string());
                        }
                    }
                }
            }
        }

        for (peer, who, want_name, want_uuid, own_uuid) in [
            (addr_a, "A (ticket slot 0, Alpha)", "Bravo", UUID_B, UUID_A),
            (addr_b, "B (ticket slot 1, Bravo)", "Alpha", UUID_A, UUID_B),
        ] {
            let got = per_peer_profiles.get(&peer).cloned().unwrap_or_default();
            if got.len() != 1 {
                failures.push(format!(
                    "(2) ROUTING: peer {who} received {} op54 opponent profiles, want exactly \
                     1: {got:?}",
                    got.len(),
                ));
                continue;
            }
            if !(got[0].contains(want_name) && got[0].contains(want_uuid)) {
                failures.push(format!(
                    "(2) ROUTING: peer {who} received the WRONG opponent profile. It must be \
                     {want_name} ({want_uuid}); the round-start burst was routed by ENet \
                     admission order instead of ticket order. Got: {}",
                    got[0],
                ));
            }
            if got[0].contains(own_uuid) {
                failures.push(format!(
                    "(2) ROUTING: peer {who} was handed a profile carrying its OWN character \
                     UUID ({own_uuid}) — this is the 'the opponent looks like me' bug. Got: {}",
                    got[0],
                ));
            }
        }

        assert!(
            failures.is_empty(),
            "ENet admission order ≠ ticket order must NOT change who is which fighter.\n  {}",
            failures.join("\n  "),
        );
    }

    /// Phase 2 — the LIVE ENet path (`admit_connection`), where op-0x38 carries no
    /// psid, plus a client that never sends an identity-bearing frame.
    ///
    /// Two properties, and they pull against each other, which is why they are one
    /// test: the identity burst must be HELD while a peer's fighter slot is unknown
    /// (otherwise it is addressed by admission order — the appearance-swap bug), and
    /// the hold must be BOUNDED (otherwise one silent client wedges the match open
    /// forever and both players sit at "Setting up…" until the sweep reclaims them).
    #[test]
    fn unbound_peers_hold_the_burst_then_fall_back_within_the_grace_window() {
        let reg = MatchRegistry::new(4);
        let gsid = Uuid::new_v4();
        assert!(reg.allocate(
            &[PSID_A.to_string(), PSID_B.to_string()],
            vec![ident_loadout("Alpha", UUID_A), ident_loadout("Bravo", UUID_B)],
            gsid,
        ));

        let addr_a: SocketAddr = "127.0.0.1:42000".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:42001".parse().unwrap();
        // The real ENet handshake: pubkey only, no psid. Neither peer can be bound.
        reg.admit_connection(addr_a, &[3u8; 32]).expect("A admitted");
        reg.admit_connection(addr_b, &[5u8; 32]).expect("B admitted");
        {
            let matches = reg.matches.lock().unwrap();
            let m = matches.get(&gsid).unwrap();
            assert!(
                m.peer_to_slot.is_empty(),
                "op-0x38 carries no psid, so neither peer can be bound at admit yet: {:?}",
                m.peer_to_slot,
            );
        }

        // --- Held: inside the grace window, with no identity frame, NOTHING goes out.
        // The FSM stays in Connecting because tick_matches reports 0 connected peers.
        let t0 = Instant::now();
        let mut early = 0usize;
        for i in 0..=25u32 {
            // t0 .. t0 + 2.5 s (< SLOT_BIND_GRACE = 3 s)
            early += reg.tick_matches(t0 + Duration::from_millis(100) * i).len();
        }
        assert_eq!(
            early, 0,
            "the round-start identity burst must be HELD while a peer's fighter slot is \
             unknown — emitting it now would address it by ENet admission order"
        );
        {
            let matches = reg.matches.lock().unwrap();
            assert_eq!(
                matches.get(&gsid).unwrap().instance.state_name(),
                "Connecting",
                "still waiting for authoritative slot binding"
            );
        }

        // --- Bounded: past SLOT_BIND_GRACE the registry commits the FIFO order (loudly)
        // and the match proceeds. A silent client must never be able to hang a match.
        let mut later = 0usize;
        for i in 26..=90u32 {
            later += reg.tick_matches(t0 + Duration::from_millis(100) * i).len();
        }
        assert!(
            later > 0,
            "past the {SLOT_BIND_GRACE:?} grace the match must PROCEED on the FIFO fallback, \
             not hang forever waiting for a PlayerInfo that is never coming"
        );

        let matches = reg.matches.lock().unwrap();
        let m = matches.get(&gsid).unwrap();
        assert!(
            m.fallback_logged,
            "the fallback is a correctness violation and must have been logged as an error"
        );
        // The fallback is committed explicitly, so the rest of the match is at least
        // SELF-CONSISTENT: two peers, two distinct slots, nothing left implicit.
        assert_eq!(m.peer_to_slot.get(&addr_a), Some(&0), "FIFO: A admitted first → slot 0");
        assert_eq!(m.peer_to_slot.get(&addr_b), Some(&1), "FIFO: B admitted second → slot 1");
        assert_ne!(m.instance.state_name(), "Connecting", "the match left the connect gate");
    }

    /// A single-peer match (solo / vs-bot) has exactly ONE possible assignment, so it
    /// must be bound at admit and must NOT pay the grace window — the gate is for
    /// disambiguating two peers, and a solo player should see no added latency.
    #[test]
    fn single_peer_match_binds_at_admit_and_is_never_held() {
        let reg = MatchRegistry::new(4);
        let gsid = Uuid::new_v4();
        assert!(reg.allocate_with_bots(
            &[PSID_A.to_string()],
            vec![ident_loadout("Alpha", UUID_A), ident_loadout("Botty", UUID_B)],
            gsid,
            1, // one bot fighter, one real peer slot
        ));
        let addr_a: SocketAddr = "127.0.0.1:43000".parse().unwrap();
        reg.admit_connection(addr_a, &[11u8; 32]).expect("admitted");
        {
            let matches = reg.matches.lock().unwrap();
            let m = matches.get(&gsid).unwrap();
            assert_eq!(
                m.peer_to_slot.get(&addr_a),
                Some(&0),
                "one peer slot ⇒ only one possible fighter slot ⇒ bind it at admit"
            );
        }
        // First tick already produces the burst — no grace window paid.
        let t0 = Instant::now();
        assert!(
            !reg.tick_matches(t0).is_empty(),
            "a solo match must start on the first tick after its peer connects"
        );
    }
}
