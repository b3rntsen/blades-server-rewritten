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

use log::{info, warn};
use rand::RngExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use arena_proto::{
    CryptoCtx, GameMessageId, chacha20_legacy_xor, first_opcode_in_plaintext, reconstruct_plaintext,
    x25519_public, x25519_shared,
};

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
    #[allow(dead_code)]
    game_session_id: Uuid,
    capacity: usize,
    players: Vec<PlayerConn>,
    instance: MatchInstance,
    _permit: OwnedSemaphorePermit,
}

pub struct MatchRegistry {
    semaphore: Arc<Semaphore>,
    pending: Mutex<HashMap<String, Uuid>>, // player_session_id -> game_session_id
    matches: Mutex<HashMap<Uuid, Match>>,  // game_session_id -> Match
    addr_index: Mutex<HashMap<SocketAddr, Uuid>>, // connected peer -> its match
    pub max_matches: usize,
}

impl MatchRegistry {
    pub fn new(max_matches: usize) -> Arc<Self> {
        Arc::new(MatchRegistry {
            semaphore: Arc::new(Semaphore::new(max_matches)),
            pending: Mutex::new(HashMap::new()),
            matches: Mutex::new(HashMap::new()),
            addr_index: Mutex::new(HashMap::new()),
            max_matches,
        })
    }

    /// Matchmaker: reserve ONE capacity slot for a new match and register the
    /// `playerSessionId`(s) it will advertise (1 = solo/bot, 2 = a paired PvP
    /// match) against `game_session_id`. Returns false at capacity.
    pub fn allocate(&self, player_session_ids: &[String], game_session_id: Uuid) -> bool {
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
        let capacity = player_session_ids.len().max(1);
        self.matches.lock().unwrap().insert(
            game_session_id,
            Match {
                game_session_id,
                capacity,
                players: Vec::with_capacity(capacity),
                instance: MatchInstance::new(capacity),
                _permit: permit,
            },
        );
        let mut pending = self.pending.lock().unwrap();
        for psid in player_session_ids {
            pending.insert(psid.clone(), game_session_id);
        }
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

        let mut replies = Vec::new();
        if let Some(op) = opcode {
            // The raw path can only answer the addressed peer, so emit only the
            // s2c the FSM targets at the sender (== all of it for a solo match).
            for (target, out_op, body) in m.instance.on_c2s(sender, op) {
                if target != sender {
                    continue;
                }
                let mut plain = Vec::with_capacity(2 + body.len());
                plain.push(0xBE);
                plain.push(out_op);
                plain.extend_from_slice(&body);
                let seq = m.instance.next_seq();
                let c = &m.players[sender].crypto;
                replies.push(crate::arena::udp::build_send_reliable(0, seq, c, &plain));
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

        let mut replies: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        if let Some(op) = opcode {
            for (target, out_op, body) in m.instance.on_c2s(sender, op) {
                let Some(tp) = m.players.get(target) else {
                    continue; // target not connected yet — drop (UDP-correct)
                };
                let mut s2c = Vec::with_capacity(2 + body.len());
                s2c.push(0xBE); // s2c marker (NetTransportMessage.MAGIC_HEADER)
                s2c.push(out_op);
                s2c.extend_from_slice(&body);
                chacha20_legacy_xor(&mut s2c, &tp.crypto.key, &tp.crypto.nonce);
                replies.push((tp.addr, s2c));
            }
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

/// Per-match authoritative state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MatchState {
    Connecting,
    InProgress,
    Finished,
}

/// Per-match state machine, now **multi-player**: `on_c2s` takes the sender's slot
/// and returns s2c targeted at specific players (self and/or opponent), which is
/// how A's action reaches B.
///
/// NOTE: the exact server→client message *flow* (which s2c follows which c2s, the
/// body layouts) is still being mapped (`docs/arena-protocol-spec.md` §6). The
/// rules below are a well-formed PLACEHOLDER that exercises the full two-player
/// wire path end-to-end (both ready → both spawn; a command relays to the
/// opponent; concede ends for all). Swap the rules in as the flow is captured;
/// the pairing/relay plumbing is unchanged.
pub struct MatchInstance {
    state: MatchState,
    capacity: usize,
    ready: [bool; 2],
    s2c_seq: u16,
}

impl MatchInstance {
    fn new(capacity: usize) -> Self {
        MatchInstance {
            state: MatchState::Connecting,
            capacity,
            ready: [false; 2],
            s2c_seq: 0,
        }
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.s2c_seq;
        self.s2c_seq = self.s2c_seq.wrapping_add(1);
        s
    }

    fn state_name(&self) -> &'static str {
        match self.state {
            MatchState::Connecting => "Connecting",
            MatchState::InProgress => "InProgress",
            MatchState::Finished => "Finished",
        }
    }

    /// Drive the FSM on a decoded c2s opcode from player `sender`; return
    /// `(target_slot, out_op, body)` s2c messages to deliver.
    fn on_c2s(&mut self, sender: usize, opcode: u8) -> Vec<(usize, u8, Vec<u8>)> {
        use GameMessageId as G;
        let mut out = Vec::new();
        match (self.state, GameMessageId::from_u8(opcode)) {
            // Loadout ready → mark this player ready; when ALL players are ready,
            // start the match: greet every player + spawn every avatar to everyone.
            (MatchState::Connecting, Some(G::PlayerLoadoutReady)) => {
                if sender < self.ready.len() {
                    self.ready[sender] = true;
                }
                if (0..self.capacity).all(|i| self.ready.get(i).copied().unwrap_or(false)) {
                    self.state = MatchState::InProgress;
                    for p in 0..self.capacity {
                        out.push((p, G::PlayerWelcome as u8, vec![]));
                        for avatar in 0..self.capacity {
                            out.push((p, G::PlayerSpawnAvatar as u8, vec![avatar as u8]));
                        }
                    }
                }
            }
            // An in-match command → relay it to the opponent(s) as a state change
            // (carrying the actor's slot); this is the core A→B relay.
            (MatchState::InProgress, Some(G::PlayerCommand)) => {
                for p in 0..self.capacity {
                    if p != sender {
                        out.push((p, G::PlayerStateChange as u8, vec![sender as u8]));
                    }
                }
            }
            // Concede → end the match for everyone.
            (MatchState::InProgress, Some(G::ConcedeMatch)) => {
                self.state = MatchState::Finished;
                for p in 0..self.capacity {
                    out.push((p, G::MatchEndMatchMsg as u8, vec![]));
                }
            }
            _ => {}
        }
        out
    }
}
