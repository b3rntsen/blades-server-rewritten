//! Live arena ENet host — the **real-client** path.
//!
//! Transport: **`rusty_enet`** (pure-Rust ENet port). Chosen over the C-backed
//! `tokio-enet` because that crate's socket layer is Linux-only
//! (`socket2::Type::cloexec`) and fails to build on macOS — blocking local dev.
//! `rusty_enet` is cross-platform, transport-agnostic (it drives our own UDP
//! socket), and inspectable — so when Blades' ENet header-flag quirk (`0x4000`
//! sentTime vs vanilla `0x8000`, per `arena-protocol-spec.md` §5) bit interop, we
//! patched it on the wire in [`BladesEnetSocket`] (below) instead of forking the
//! crate. The retail client ships `libenet.so` → otherwise-standard ENet.
//!
//! **What this does.** `rusty_enet` owns the ENet protocol (CONNECT/VERIFY,
//! reliability, ACKs, sequencing, fragmentation, ping/timeout). On top:
//!   - On `Connect`, the ENet session is up but there is no crypto yet; we record
//!     the peer's `(addr → PeerID)` so we can later send to it by address.
//!   - The first reliable channel-0 packet is the app handshake — client X25519
//!     pubkey(32) ‖ `playerSessionId`. `registry.admit` joins the player to the
//!     match the matchmaker pre-allocated (ECDH + nonce) and we reply server
//!     pubkey(32) ‖ nonce(8).
//!   - Later packets are `chacha20(marker ‖ opcode ‖ body)`.
//!     `registry.handle_live_user_data` decrypts under the sender's key, drives
//!     the shared match FSM, and returns replies **targeted at specific players**
//!     (`(addr, encrypted_user_data)`) — so player A's action is relayed to
//!     player B. We send each to the right peer via its `PeerID`.
//!
//! The app handshake here is still our **own** minimal framing (the retail
//! connect-phase bytes are #T5, being captured); when pinned, only the handshake
//! parse changes — the pairing/relay/crypto path is unchanged.
//!
//! Concurrency: `rusty_enet`'s `service()` is synchronous over a non-blocking
//! socket, so the host runs on its **own OS thread** and calls the (sync,
//! `Mutex`-based) [`MatchRegistry`] directly — never an `.await`, never a lock
//! held across one.

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use log::{debug, error, info, warn};
use rusty_enet::{
    Event, Host, HostSettings, MTU_MAX, Packet, PacketReceived, PeerID, Socket, SocketOptions,
};

use crate::ServerGlobal;
use crate::arena::match_registry::MatchRegistry;

/// A [`Socket`] wrapper that translates Blades' ENet protocol-header flag convention
/// to/from the vanilla ENet that `rusty_enet` implements, transparently on the wire.
///
/// Blades' bundled `libenet` places the **SENT_TIME** flag at bit `0x4000`, where
/// vanilla ENet uses `0x8000` and reads `0x4000` as **COMPRESSED**. Without this,
/// `rusty_enet` sees every inbound client datagram as "compressed", finds no
/// decompressor, and silently drops it (`c/protocol.rs`: `return false`) — so the
/// ENet CONNECT never completes and the client times out ("error 2"). Confirmed on
/// the wire: the captured CONNECT header is `0x4FFF` (bit `0x4000` = Blades SENT_TIME).
///
/// We rewrite ONLY the two flag bits in the header's high byte (the 2-byte big-endian
/// `peerID|flags`); the session id (`0x30`) and peer-id high nibble (`0x0F`) are
/// preserved. Applied on every datagram both directions, so `rusty_enet` always sees
/// vanilla framing and the client always sees Blades. (Blades uses no ENet checksum —
/// the captured CONNECT has the command byte immediately after the 4-byte header — so
/// rewriting the flag byte cannot invalidate a checksum.)
struct BladesEnetSocket(UdpSocket);

/// Inbound (client → server): move Blades SENT_TIME (`0x4000`) to the vanilla
/// position (`0x8000`). Clearing the old bit also guarantees the vanilla COMPRESSED
/// flag (`0x4000`) is never left set — we run no decompressor.
fn header_blades_to_vanilla(b0: u8) -> u8 {
    (b0 & 0x3F) | ((b0 & 0x40) << 1)
}
/// Outbound (server → client): move vanilla SENT_TIME (`0x8000`) back to Blades (`0x4000`).
fn header_vanilla_to_blades(b0: u8) -> u8 {
    (b0 & 0x3F) | ((b0 & 0x80) >> 1)
}

impl BladesEnetSocket {
    fn new(socket: UdpSocket) -> Self {
        Self(socket)
    }
    #[cfg(test)]
    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.0.local_addr()
    }
}

impl Socket for BladesEnetSocket {
    type Address = std::net::SocketAddr;
    type Error = std::io::Error;

    fn init(&mut self, options: SocketOptions) -> Result<(), Self::Error> {
        Socket::init(&mut self.0, options)
    }

    fn send(&mut self, address: Self::Address, buffer: &[u8]) -> Result<usize, Self::Error> {
        // rusty_enet hands us vanilla framing; rewrite the flag byte to Blades' before
        // it goes on the wire. (Copy — `buffer` is owned by rusty_enet.)
        let mut out = buffer.to_vec();
        if let Some(b0) = out.first_mut() {
            *b0 = header_vanilla_to_blades(*b0);
        }
        Socket::send(&mut self.0, address, &out)
    }

    fn receive(
        &mut self,
        buffer: &mut [u8; MTU_MAX],
    ) -> Result<Option<(Self::Address, PacketReceived)>, Self::Error> {
        let received = Socket::receive(&mut self.0, buffer)?;
        if let Some((_, PacketReceived::Complete(n))) = &received {
            if *n >= 1 {
                buffer[0] = header_blades_to_vanilla(buffer[0]);
            }
        }
        Ok(received)
    }
}

/// The arena ENet host: a [`Host`] over the flag-translating [`BladesEnetSocket`].
type ArenaHost = Host<BladesEnetSocket>;

/// Bind the arena UDP socket and run the ENet host on a dedicated thread.
pub async fn run_enet_host(globals: Arc<ServerGlobal>) -> anyhow::Result<()> {
    let port = globals.arena.config.udp_port;
    let registry = globals.arena.registry.clone();

    let socket = UdpSocket::bind(("0.0.0.0", port))
        .map_err(|e| anyhow::anyhow!("arena-enet: bind udp/{port}: {e}"))?;
    // One ENet peer per connected client: up to 2 players per match, plus headroom.
    let peer_limit = (registry.max_matches * 2).clamp(2, 256);

    thread::Builder::new()
        .name("arena-enet".into())
        .spawn(move || serve(socket, registry, peer_limit))
        .map_err(|e| anyhow::anyhow!("arena-enet: spawn host thread: {e}"))?;

    info!("arena-enet: live host bound udp/{port} (rusty_enet, peer_limit {peer_limit})");
    Ok(())
}

/// The ENet service loop (own thread): drain queued events each tick, flush, yield.
fn serve(socket: UdpSocket, registry: Arc<MatchRegistry>, peer_limit: usize) {
    let mut host = match Host::new(
        BladesEnetSocket::new(socket),
        HostSettings {
            peer_limit,
            ..Default::default()
        },
    ) {
        Ok(h) => h,
        Err(e) => {
            error!("arena-enet: Host::new failed: {e:?}");
            return;
        }
    };
    // addr -> PeerID, so we can send to the *opponent* (not just the event peer).
    let mut peer_at: HashMap<std::net::SocketAddr, PeerID> = HashMap::new();

    let mut last_housekeep = std::time::Instant::now();
    loop {
        while pump(&mut host, &registry, &mut peer_at) {}
        let now = std::time::Instant::now();
        // Server-initiated combat output (flow-control heartbeat, damage, etc.).
        // The retail ENet CHANNEL is chosen per-message by (carrier, GameMessageId)
        // in `messages::retail_channel` and threaded here from the registry (computed
        // on the PLAINTEXT before encryption). s506 map: the big op54 OpponentLoadout
        // profile + MatchEnd ride ch4; PlayerStatsUpdate/PlayerDestroyedStatUpdate
        // (small) ride ch1; everything else ch0. (The old "route by ciphertext length
        // >1000 ⇒ ch4 else ch0" heuristic NEVER used ch1, so the small stat-word
        // PlayerStatsUpdate (GMID 65) went on ch0 — a channel the client doesn't read
        // those on — and the client's `OnUserMessage` receive path stalled.)
        for (addr, channel, bytes) in registry.tick_matches(now) {
            if bytes.len() > 1000 {
                info!(
                    "ARENA-DIAG send (tick) → {addr}: {} B on channel {channel} (op54 PROFILE; rusty_enet will fragment)",
                    bytes.len()
                );
            }
            send_to(&mut host, &peer_at, &addr, channel, &bytes);
        }
        // Post-match: a match whose FSM reached the terminal MatchState
        // (DisconnectingPlayersAfterMatch=19 → Finished) is retired here, and we ENet-
        // DISCONNECT its peer(s) — the literal meaning of state 19.
        //
        // It MUST be `disconnect_later`, not `disconnect`. The comment here used to
        // claim `disconnect(0)` was "graceful: rusty_enet flushes the already-queued
        // reliable end-of-match frames and THEN tears the connection down". It does the
        // opposite: `enet_peer_disconnect` begins with `enet_peer_reset_queues(peer)`,
        // which THROWS AWAY every outgoing command the peer has not yet acknowledged.
        // `enet_peer_disconnect_later` is the one that waits for the outgoing queue to
        // drain and only then disconnects.
        //
        // What that cost (report #163): the end-of-match op49 carries the recipient's
        // whole character record — 41 KB for a level-61 player with a full backpack —
        // so it leaves here as ~30 fragments of a reliable packet. Sixteen seconds
        // later the terminal walk finishes and we reset the queue, discarding whatever
        // fragments were still in flight. The client cannot reassemble a message it
        // only half received, so it never raises the victory overlay, never draws the
        // Continue button, and sits in the post-match third-person camera until the
        // player force-quits — which is the reported bug, verbatim, on every match.
        // Bigger inventory, bigger card, more fragments to lose.
        for addr in registry.take_finished_peers() {
            if let Some(&pid) = peer_at.get(&addr) {
                let peer = host.peer_mut(pid);
                // Logged at the disconnect so a recurrence can be read off the server
                // instead of inferred: a client that never acknowledged the results
                // card shows up as loss against a healthy round-trip time.
                info!(
                    "arena-enet: match finished → disconnect_later peer {addr}                      (packets sent {}, lost {}, rtt {:?}) — draining the queue first",
                    peer.packets_sent(),
                    peer.packets_lost(),
                    peer.round_trip_time(),
                );
                peer.disconnect_later(0);
                peer_at.remove(&addr);
            }
        }
        // DEBUG/experimental: drain any hand-crafted frames queued by the
        // token-gated /arena/debug/inject route and send them down the SAME
        // encrypt+send path (already encrypted under the target peer's key; the
        // retail channel is computed from the injected plaintext in the registry).
        // Inert when nothing is queued.
        for (addr, channel, bytes, res) in registry.drain_debug_injections() {
            info!(
                "arena-enet DEBUG inject → {addr} slot {} ({} B, nonce {}, channel {channel})",
                res.slot, res.ciphertext_len, res.nonce_hex
            );
            send_to(&mut host, &peer_at, &addr, channel, &bytes);
        }
        host.flush();
        // Housekeeping ~every 5 s: reclaim leaked match slots (so the registry
        // never sticks "at capacity" after abandoned connects) + a liveness line,
        // so a stuck or zero-connect host is obvious from the logs alone.
        if now.saturating_duration_since(last_housekeep) >= Duration::from_secs(5) {
            registry.sweep_expired(now);
            debug!(
                "arena-enet: alive — peers {}, matches {}, permits {}/{} free",
                peer_at.len(),
                registry.active_count(),
                registry.available_permits(),
                registry.max_matches
            );
            last_housekeep = now;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

/// One ENet event: extract owned data (releasing the borrow on `host`), then route
/// through the registry and send any replies to their target peers. Returns true
/// if an event was handled, false when the queue is drained (or on error).
fn pump(
    host: &mut ArenaHost,
    registry: &MatchRegistry,
    peer_at: &mut HashMap<std::net::SocketAddr, PeerID>,
) -> bool {
    // Extract everything we need, then drop the event so `host` is free to send.
    let action = match host.service() {
        Ok(Some(Event::Connect { peer, .. })) => Act::Connect(peer.id(), peer.address()),
        Ok(Some(Event::Disconnect { peer, .. })) => Act::Disconnect(peer.id(), peer.address()),
        Ok(Some(Event::Receive { peer, packet, .. })) => {
            Act::Receive(peer.id(), peer.address(), packet.data().to_vec())
        }
        Ok(None) => return false,
        Err(e) => {
            warn!("arena-enet: service error: {e}");
            return false;
        }
    };

    match action {
        Act::Connect(pid, addr) => {
            if let Some(addr) = addr {
                peer_at.insert(addr, pid);
            }
            info!("arena-enet: peer connected ({addr:?})");
        }
        Act::Disconnect(pid, addr) => {
            info!("arena-enet: peer disconnected ({addr:?})");
            if let Some(addr) = addr {
                // A finished match is retired before its graceful ENet disconnect
                // completes. The client may re-queue immediately and reuse the same
                // UDP source address for a NEW PeerID. In that case the delayed
                // Disconnect event belongs to the old peer, while `addr` already
                // names the new match in both `peer_at` and the registry. Removing by
                // address alone tears down the fresh match and leaves the client in
                // the arena camera waiting forever.
                //
                // Only the PeerID currently owning this address may mutate registry
                // state. A missing owner is also stale: server-initiated post-match
                // retirement deliberately removes `peer_at` before the eventual
                // Disconnect event arrives.
                if !disconnect_still_owns_address(peer_at, &addr, &pid) {
                    info!(
                        "arena-enet: ignoring stale disconnect for peer {pid:?} at {addr}; \
                         address is absent or already owned by a newer peer"
                    );
                    return true;
                }
                // If the peer left a live match, its opponent wins by concession;
                // send the immediate victory frames down the same encrypt+send path.
                // The match-end walk then rides the tick loop as for a normal win.
                let now = std::time::Instant::now();
                for (target, channel, bytes) in registry.peer_departed(&addr, now) {
                    send_to(host, peer_at, &target, channel, &bytes);
                }
                peer_at.remove(&addr);
            }
        }
        Act::Receive(pid, Some(addr), data) => {
            peer_at.insert(addr, pid);
            handle_packet(host, registry, peer_at, addr, &data);
        }
        Act::Receive(_, None, _) => {
            debug!("arena-enet: receive from a peer with no address; dropping");
        }
    }
    true
}

enum Act {
    Connect(PeerID, Option<std::net::SocketAddr>),
    Disconnect(PeerID, Option<std::net::SocketAddr>),
    Receive(PeerID, Option<std::net::SocketAddr>, Vec<u8>),
}

/// Whether a disconnect event still belongs to the peer currently registered at an
/// address. Generic over the owner token so the address-reuse race can be tested
/// without manufacturing a rusty_enet `PeerID`.
fn disconnect_still_owns_address<T: PartialEq>(
    peer_at: &HashMap<std::net::SocketAddr, T>,
    addr: &std::net::SocketAddr,
    disconnecting: &T,
) -> bool {
    peer_at
        .get(addr)
        .is_some_and(|current| current == disconnecting)
}

/// Route a received SEND payload: active peer → decrypt + FSM + relay; unknown
/// peer → the op-0x38 connect handshake (parse the client pubkey →
/// `admit_connection` → reply our pubkey + nonce).
fn handle_packet(
    host: &mut ArenaHost,
    registry: &MatchRegistry,
    peer_at: &HashMap<std::net::SocketAddr, PeerID>,
    addr: std::net::SocketAddr,
    data: &[u8],
) {
    // Parse the plaintext retail key exchange BEFORE consulting addr_index. Mobile
    // clients routinely reuse the same UDP source address for their next queue. If
    // that address still belongs to the preceding match, trying to decrypt this
    // plaintext frame with the old key produces a bad marker and the fresh match
    // remains forever at "Opponent Found: Setting Up".
    if let Some((conn_id, client_pub)) = parse_key_exchange(data) {
        let was_active = registry.is_active(&addr);
        if !was_active || registry.has_fresh_connection_reservation(addr) {
            if was_active {
                info!(
                    "arena-enet: fresh op-0x38 from active address {addr} — retiring the old match generation before re-queue"
                );
                let now = std::time::Instant::now();
                for (target, channel, bytes) in registry.peer_departed(&addr, now) {
                    send_to(host, peer_at, &target, channel, &bytes);
                }
            }
            match registry.admit_connection(addr, &client_pub) {
                Some((server_pk, nonce)) => {
                    let mut reply = Vec::with_capacity(55);
                    reply.extend_from_slice(&[0xBE, 0x38]);
                    reply.extend_from_slice(&conn_id);
                    reply.extend_from_slice(&[0, 0, 0, 1]);
                    reply.extend_from_slice(&[0x01, 0x20]);
                    reply.extend_from_slice(&server_pk);
                    reply.push(0x08);
                    reply.extend_from_slice(&nonce);
                    send_to(host, peer_at, &addr, 0, &reply);
                    info!("arena-enet: {addr} admitted (op-0x38 key exchange)");
                }
                None => warn!(
                    "arena-enet: {addr} sent op-0x38 handshake but NO fresh match has a free slot \
                     (active {}, permits {} free)",
                    registry.active_count(),
                    registry.available_permits()
                ),
            }
            return;
        }
        debug!(
            "arena-enet: duplicate op-0x38 from active address {addr} with no fresh reservation; keeping the current match generation"
        );
        return;
    }

    if registry.is_active(&addr) {
        if let Some(out) = registry.handle_live_user_data(&addr, data) {
            match out.opcode {
                Some(op) => info!("arena-enet: {addr} → GameMessageId {op} [{}]", out.state),
                // c2s=0x84, s2c=0xBE, 0xAC also valid. Any other byte after decrypt ⇒
                // wrong key (handshake mismatch) or a mis-routed peer (e.g. docker-proxy
                // SNAT collision) — make it loud instead of a silent drop.
                None => match out.marker {
                    Some(m) if !matches!(m, 0x84 | 0xBE | 0xAC) => warn!(
                        "arena-enet: {addr} frame decrypted to BAD marker {m:#04x} ({} B) — wrong key / mis-routed peer?",
                        data.len()
                    ),
                    _ => debug!(
                        "arena-enet: {addr} frame with no opcode ({} B, marker {:?})",
                        data.len(),
                        out.marker
                    ),
                },
            }
            // Deliver each reply to its TARGET peer (may be the opponent — e.g. the
            // relayed op54 profile in response to a PlayerLoadoutReady upload). The
            // retail ENet channel was chosen per (carrier, GameMessageId) in the
            // registry (`messages::retail_channel`, s506 map) on the plaintext before
            // encryption, and is threaded here — the profile/MatchEnd on ch4, the stat
            // words (op54-small) on ch1, everything else ch0.
            for (target_addr, channel, bytes) in &out.replies {
                send_to(host, peer_at, target_addr, *channel, bytes);
            }
        }
        return;
    }

    // Mobile/carrier NAT can change only the UDP source port after the app-level
    // key exchange. The replacement ENet peer then starts with ciphertext, not a
    // second op-0x38 handshake. Let the registry prove it owns this session by
    // decrypting to a valid marker/opcode, migrate the address, and process the
    // packet normally. The old ENet Disconnect becomes harmless because its
    // address is no longer indexed.
    if let Some(rebind) = registry.rebind_encrypted_peer(addr, data) {
        info!(
            "arena-enet: recovered mobile peer address {} → {addr}; replaying {} current-state frame(s)",
            rebind.old_addr,
            rebind.replay.len(),
        );
        for (channel, bytes) in &rebind.replay {
            send_to(host, peer_at, &addr, *channel, bytes);
        }
        handle_packet(host, registry, peer_at, addr, data);
        return;
    }

    info!(
        "arena-enet: {addr} {}B from an unknown peer — NOT an op-0x38 handshake \
         (b0={:#04x} b1={:#04x} b12={:#04x} b13={:#04x})",
        data.len(),
        data.first().copied().unwrap_or(0),
        data.get(1).copied().unwrap_or(0),
        data.get(12).copied().unwrap_or(0),
        data.get(13).copied().unwrap_or(0),
    );
}

/// Decode the fixed prefix of the retail plaintext op-0x38 key exchange.
fn parse_key_exchange(data: &[u8]) -> Option<([u8; 6], [u8; 32])> {
    if data.len() < 46
        || data[0] != 0xBE
        || data[1] != 0x38
        || data[12] != 0x01
        || data[13] != 0x20
    {
        return None;
    }
    let mut conn_id = [0u8; 6];
    conn_id.copy_from_slice(&data[2..8]);
    let mut client_pub = [0u8; 32];
    client_pub.copy_from_slice(&data[14..46]);
    Some((conn_id, client_pub))
}

/// Send a reliable packet on `channel` to the peer at `addr` (looked up by PeerID).
/// The big op54 profile uses channel 4 (retail/s486); everything else channel 0.
fn send_to(
    host: &mut ArenaHost,
    peer_at: &HashMap<std::net::SocketAddr, PeerID>,
    addr: &std::net::SocketAddr,
    channel: u8,
    bytes: &[u8],
) {
    if let Some(&pid) = peer_at.get(addr) {
        if let Some(peer) = host.get_peer_mut(pid) {
            if let Err(e) = peer.send(channel, &Packet::reliable(bytes)) {
                warn!("arena-enet: send to {addr} failed: {e:?}");
            }
            return;
        }
    }
    debug!("arena-enet: no live peer for {addr}; dropping reply");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::match_registry::{MatchRegistry, gen_keypair};
    use arena_proto::{CryptoCtx, chacha20_legacy_xor, x25519_shared};
    use std::net::SocketAddr;
    use uuid::Uuid;

    /// The ENet header-flag translation that unblocks the real client: Blades puts
    /// SENT_TIME at 0x4000 (= vanilla COMPRESSED), so the flag byte must be rewritten
    /// both directions. Anchored on the captured CONNECT header high byte 0x4F.
    #[test]
    fn enet_header_flag_translation() {
        // Captured Blades CONNECT: header 0x4FFF, high byte 0x4F (bit 0x40 = Blades
        // SENT_TIME). rusty_enet must see vanilla 0x8F (bit 0x80 = SENT_TIME).
        assert_eq!(header_blades_to_vanilla(0x4F), 0x8F);
        assert_eq!(header_vanilla_to_blades(0x8F), 0x4F);

        // The vanilla COMPRESSED bit (0x40) is NEVER left set after inbound translation,
        // for any header byte — that is the whole point (we run no decompressor).
        for b0 in 0u8..=0xFF {
            assert_eq!(header_blades_to_vanilla(b0) & 0x40, 0, "COMPRESSED clear (b0={b0:#04x})");
        }

        // Session id (0x30) + peer-id high nibble (0x0F) are preserved; the flag bit
        // round-trips.
        let blades = 0x40 | 0x35; // SENT_TIME + session/peer bits
        let vanilla = header_blades_to_vanilla(blades);
        assert_eq!(vanilla, 0x80 | 0x35);
        assert_eq!(header_vanilla_to_blades(vanilla), blades);

        // A header with no flag bits is untouched both directions.
        assert_eq!(header_blades_to_vanilla(0x2A), 0x2A);
        assert_eq!(header_vanilla_to_blades(0x2A), 0x2A);
    }

    /// A phone can finish a match, re-queue, and reuse its UDP source address before
    /// rusty_enet emits the old peer's graceful Disconnect event. The old event must
    /// not be allowed to remove the new match that now owns that address.
    #[test]
    fn stale_disconnect_cannot_remove_requeued_peer_at_same_address() {
        let addr: SocketAddr = "127.0.0.1:32001".parse().unwrap();
        let old_peer = 7_u32;
        let requeued_peer = 8_u32;
        let mut peer_at = HashMap::new();

        peer_at.insert(addr, old_peer);
        assert!(disconnect_still_owns_address(&peer_at, &addr, &old_peer));

        // Re-queue overwrites the address with a fresh ENet peer generation.
        peer_at.insert(addr, requeued_peer);
        assert!(
            !disconnect_still_owns_address(&peer_at, &addr, &old_peer),
            "the delayed old Disconnect must be ignored"
        );
        assert!(
            disconnect_still_owns_address(&peer_at, &addr, &requeued_peer),
            "the current peer may still perform a real disconnect"
        );

        // Post-match retirement removes the mapping before its own graceful
        // Disconnect arrives; that event is stale too.
        peer_at.remove(&addr);
        assert!(!disconnect_still_owns_address(
            &peer_at,
            &addr,
            &requeued_peer
        ));
    }

    #[test]
    fn retail_key_exchange_parser_accepts_only_the_plaintext_handshake() {
        let mut frame = vec![0u8; 46];
        frame[0] = 0xBE;
        frame[1] = 0x38;
        frame[2..8].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        frame[12] = 0x01;
        frame[13] = 0x20;
        frame[14..46].copy_from_slice(&[7u8; 32]);
        assert_eq!(
            parse_key_exchange(&frame),
            Some(([1, 2, 3, 4, 5, 6], [7u8; 32]))
        );

        frame[1] = 0x36;
        assert_eq!(parse_key_exchange(&frame), None, "encrypted game traffic is not a key exchange");
        assert_eq!(parse_key_exchange(&frame[..45]), None, "a truncated exchange is rejected");
    }

    /// A rusty_enet test client: connect, then send/recv reliable channel-0 frames.
    struct Client {
        host: ArenaHost,
        pid: PeerID,
        connected: bool,
        disconnected: bool,
        inbox: Vec<Vec<u8>>,
        crypto: Option<CryptoCtx>,
    }

    impl Client {
        fn connect(server: SocketAddr) -> Self {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut host = Host::new(
                BladesEnetSocket::new(sock),
                HostSettings { peer_limit: 2, ..Default::default() },
            )
            .unwrap();
            // Request 7 channels (ch0–6), matching the retail client's CONNECT
            // (channelCount=7 in s506) so the server can send on ch1/ch4/ch6.
            let pid = host.connect(server, 7, 0).unwrap().id();
            Client {
                host,
                pid,
                connected: false,
                disconnected: false,
                inbox: Vec::new(),
                crypto: None,
            }
        }
        /// Start a fresh ENet generation on the SAME host/socket. This mirrors the
        /// retail client returning to the arena lobby and immediately queueing again:
        /// the UDP source address is retained, but rusty_enet assigns a new PeerID.
        fn reconnect(&mut self, server: SocketAddr) {
            self.pid = self.host.connect(server, 7, 0).unwrap().id();
            self.connected = false;
            self.disconnected = false;
            self.inbox.clear();
            self.crypto = None;
        }
        fn addr(&self) -> SocketAddr {
            self.host.socket().local_addr().unwrap()
        }
        /// Drain client events: note Connect, collect (decrypted, if keyed) Receives.
        fn drain(&mut self) {
            while let Ok(Some(ev)) = self.host.service() {
                match ev {
                    Event::Connect { .. } => self.connected = true,
                    Event::Receive { packet, .. } => {
                        let mut d = packet.data().to_vec();
                        if let Some(c) = &self.crypto {
                            chacha20_legacy_xor(&mut d, &c.key, &c.nonce);
                        }
                        self.inbox.push(d);
                    }
                    Event::Disconnect { .. } => {
                        self.connected = false;
                        self.disconnected = true;
                    }
                }
            }
            self.host.flush();
        }
        fn send_plain(&mut self, bytes: &[u8]) {
            self.host
                .peer_mut(self.pid)
                .send(0, &Packet::reliable(bytes))
                .unwrap();
        }
        #[allow(dead_code)] // used by the Phase B combat-input tests
        fn send_enc(&mut self, marker: u8, opcode: u8) {
            let c = self.crypto.clone().expect("keyed");
            let mut ud = vec![marker, opcode];
            chacha20_legacy_xor(&mut ud, &c.key, &c.nonce);
            self.send_plain(&ud);
        }
        /// Encrypt + send a full decrypted `user_data` payload under this client's
        /// key (the round-start op58 clock-sync sends a multi-byte NetData body).
        fn send_enc_payload(&mut self, plain: &[u8]) {
            let c = self.crypto.clone().expect("keyed");
            let mut ud = plain.to_vec();
            chacha20_legacy_xor(&mut ud, &c.key, &c.nonce);
            self.send_plain(&ud);
        }
    }

    fn hs_c2s(pk: &[u8; 32]) -> Vec<u8> {
        let mut m = vec![
            0xBE, 0x38, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0, 0, 0, 0, 0x01, 0x20,
        ];
        m.extend_from_slice(pk);
        m
    }

    /// The exact production failure: matchmaking allocates a fresh match while the
    /// phone's address still indexes a stuck previous match, then the phone opens a
    /// new ENet generation and sends plaintext op-0x38 from that same address. The
    /// new exchange must be answered, not decrypted with the old match key.
    #[test]
    fn fresh_key_exchange_replaces_active_match_at_same_address() {
        let registry = MatchRegistry::new(4);
        assert!(registry.allocate_with_bots(
            &["old".to_string()],
            vec![Default::default(), Default::default()],
            Uuid::new_v4(),
            1,
        ));

        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let mut server = Host::new(
            BladesEnetSocket::new(server_sock),
            HostSettings { peer_limit: 4, ..Default::default() },
        )
        .unwrap();
        let mut peer_at = HashMap::new();
        let mut client = Client::connect(server_addr);

        for _ in 0..2000 {
            while pump(&mut server, &registry, &mut peer_at) {}
            server.flush();
            client.drain();
            if client.connected {
                break;
            }
        }
        assert!(client.connected, "first ENet generation connects");
        let (_, old_pk) = gen_keypair();
        client.send_plain(&hs_c2s(&old_pk));
        for _ in 0..2000 {
            while pump(&mut server, &registry, &mut peer_at) {}
            server.flush();
            client.drain();
            if !client.inbox.is_empty() {
                break;
            }
        }
        assert_eq!(client.inbox.first().map(Vec::len), Some(55));
        assert!(registry.is_active(&client.addr()));

        assert!(registry.allocate_with_bots(
            &["new".to_string()],
            vec![Default::default(), Default::default()],
            Uuid::new_v4(),
            1,
        ));
        assert!(registry.has_fresh_connection_reservation(client.addr()));
        client.reconnect(server_addr);
        for _ in 0..2000 {
            while pump(&mut server, &registry, &mut peer_at) {}
            server.flush();
            client.drain();
            if client.connected {
                break;
            }
        }
        assert!(client.connected, "second ENet generation connects at the same address");

        let (_, new_pk) = gen_keypair();
        client.send_plain(&hs_c2s(&new_pk));
        for _ in 0..2000 {
            while pump(&mut server, &registry, &mut peer_at) {}
            server.flush();
            client.drain();
            if !client.inbox.is_empty() {
                break;
            }
        }
        assert_eq!(
            client.inbox.first().map(Vec::len),
            Some(55),
            "the fresh plaintext key exchange receives a fresh server key/nonce reply"
        );
        assert_eq!(registry.active_count(), 1, "the stuck old solo match was retired");
        assert!(!registry.has_fresh_connection_reservation(client.addr()));
    }

    /// End-to-end reproduction of the mobile setup failure: the app-level crypto
    /// session survives, but ENet reconnects from a new UDP source port after the
    /// authoritative server has already reached InRound. Reliable commands queued
    /// on the old PeerID cannot cross that boundary, so the replacement must receive
    /// an explicit current-state replay on its own ENet generation.
    #[test]
    fn encrypted_mobile_rebind_receives_live_state_replay_over_new_enet_peer() {
        let registry = MatchRegistry::new(4);
        assert!(registry.allocate_with_bots(
            &["mobile-player".to_string()],
            vec![Default::default(), Default::default()],
            Uuid::new_v4(),
            1,
        ));

        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let mut server = Host::new(
            BladesEnetSocket::new(server_sock),
            HostSettings {
                peer_limit: 4,
                ..Default::default()
            },
        )
        .unwrap();
        let mut peer_at = HashMap::new();
        let mut old = Client::connect(server_addr);

        for _ in 0..2000 {
            while pump(&mut server, &registry, &mut peer_at) {}
            server.flush();
            old.drain();
            if old.connected {
                break;
            }
        }
        assert!(old.connected);

        let (client_sk, client_pk) = gen_keypair();
        old.send_plain(&hs_c2s(&client_pk));
        for _ in 0..2000 {
            while pump(&mut server, &registry, &mut peer_at) {}
            server.flush();
            old.drain();
            if !old.inbox.is_empty() {
                break;
            }
        }
        let reply = old.inbox.first().expect("key exchange reply");
        let mut server_pk = [0u8; 32];
        server_pk.copy_from_slice(&reply[14..46]);
        let mut nonce = [0u8; 8];
        nonce.copy_from_slice(&reply[47..55]);
        old.crypto = Some(CryptoCtx {
            key: x25519_shared(&client_sk, &server_pk),
            nonce,
        });
        old.inbox.clear();

        let mut vnow = std::time::Instant::now();
        for _ in 0..160 {
            while pump(&mut server, &registry, &mut peer_at) {}
            vnow += Duration::from_millis(250);
            for (addr, channel, bytes) in registry.tick_matches(vnow) {
                send_to(&mut server, &peer_at, &addr, channel, &bytes);
            }
            server.flush();
            old.drain();
            if old.inbox.iter().any(|m| m.ends_with(b"StateTimeout")) {
                break;
            }
        }
        assert_eq!(registry.debug_list()[0].phase, "StateTimeout");

        let mut replacement = Client::connect(server_addr);
        replacement.crypto = old.crypto.clone();
        assert_ne!(replacement.addr(), old.addr(), "new mobile source port");
        for _ in 0..2000 {
            while pump(&mut server, &registry, &mut peer_at) {}
            server.flush();
            replacement.drain();
            if replacement.connected {
                break;
            }
        }
        assert!(replacement.connected);

        let mut ack = crate::arena::combat::messages::match_state_change_ack(560, "StateTimeout");
        ack[0] = 0x84;
        replacement.send_enc_payload(&ack);
        for _ in 0..2000 {
            while pump(&mut server, &registry, &mut peer_at) {}
            server.flush();
            old.drain();
            replacement.drain();
            let saw_inround = replacement.inbox.iter().any(|m| {
                m.get(1) == Some(&0x35)
                    && arena_proto::parse_netdata(&m[2..]).int(5)
                        == Some(crate::arena::combat::state::MatchState::InRound as i64)
            });
            let saw_flow = replacement
                .inbox
                .iter()
                .any(|m| m.ends_with(b"StateTimeout"));
            if saw_inround && saw_flow {
                break;
            }
        }
        assert!(replacement.inbox.iter().any(|m| {
            m.get(1) == Some(&0x35)
                && arena_proto::parse_netdata(&m[2..]).int(5)
                    == Some(crate::arena::combat::state::MatchState::InRound as i64)
        }));
        assert!(replacement
            .inbox
            .iter()
            .any(|m| m.ends_with(b"StateTimeout")));
        let peer = &registry.debug_list()[0].peers[0];
        assert_eq!(peer.rebind_count, 1);
        assert_eq!(peer.last_match_state_ack.as_deref(), Some("StateTimeout"));
    }

    /// Two rusty_enet clients, a shared 2-player match: both CONNECT, op-0x38
    /// handshake, and once both are admitted the TICK drives match-start — each
    /// client receives `BackendMatchCreated` + a combat-screen per avatar,
    /// correctly encrypted under its OWN key. Proves pairing (shared match) +
    /// tick-driven s2c delivery + per-target crypto end-to-end. (Combat-action
    /// relay returns as real swipe→damage in Phase B.)
    #[test]
    fn two_consecutive_two_player_matches_reuse_client_addresses() {
        let _ = env_logger::builder().is_test(true).try_init();

        let registry = MatchRegistry::new(4);
        let gsid = Uuid::new_v4();
        let (psid_a, psid_b) = ("psess-a".to_string(), "psess-b".to_string());
        assert!(registry.allocate(&[psid_a, psid_b], Vec::new(), gsid)); // matchmaker pairing (FIFO-bound below)

        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let mut server = Host::new(
            BladesEnetSocket::new(server_sock),
            HostSettings { peer_limit: 16, ..Default::default() },
        )
        .unwrap();
        let mut peer_at = HashMap::new();

        let mut a = Client::connect(server_addr);
        let mut b = Client::connect(server_addr);

        // Drive server + both clients until a predicate holds, or panic after budget.
        // Handshake phase: drive I/O only (NO tick) — so match-start doesn't fire
        // before each client has computed its key (else it'd arrive as ciphertext
        // and be lost on the inbox clear).
        macro_rules! pump_io {
            ($cond:expr, $msg:expr) => {{
                let mut ok = false;
                for _ in 0..2000 {
                    while pump(&mut server, &registry, &mut peer_at) {}
                    server.flush();
                    a.drain();
                    b.drain();
                    if $cond {
                        ok = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(ok, $msg);
            }};
        }
        // Lifecycle phase: also drive the per-match tick, exactly as the real
        // serve loop does (this is what emits the flow-control + combat s2c).
        //
        // The tick clock is VIRTUAL (advances 250 ms per iteration), not wall-clock:
        // the round-start FSM staggers BackendMatchCreation(5) ~4 s after the spawns
        // (SPAWN_HANDSHAKE_HOLD), then walks the MatchState 6→7→11→12→13 over ~22 s
        // (MATCH_STATE_ROUND0_PROGRESSION) into the live round (StateTimeout). A 1 ms
        // real sleep × 2000 iterations only covers ~2 s of real time — never enough to
        // reach the live round — so we advance a synthetic `Instant` instead (250 ms ×
        // 2000 = 500 s virtual), reaching the staggered states deterministically.
        let mut vnow = std::time::Instant::now();
        macro_rules! pump_tick {
            ($cond:expr, $msg:expr) => {{
                let mut ok = false;
                for _ in 0..2000 {
                    while pump(&mut server, &registry, &mut peer_at) {}
                    vnow += Duration::from_millis(250);
                    for (addr, channel, bytes) in registry.tick_matches(vnow) {
                        send_to(&mut server, &peer_at, &addr, channel, &bytes);
                    }
                    server.flush();
                    a.drain();
                    b.drain();
                    if $cond {
                        ok = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(ok, $msg);
            }};
        }

        // 1. Both ENet sessions connect.
        pump_io!(a.connected && b.connected, "both clients connect");

        // 2. op-0x38 connect handshake. c2s: BE 38 | conn_id(6) | 00000000 | 01 20 |
        //    client_pubkey(32). s2c reply also carries 08 | nonce(8) after the pubkey.
        //    No psid in the handshake — admit_connection FIFO-binds to the open match.
        let (sk_a, pk_a) = gen_keypair();
        let (sk_b, pk_b) = gen_keypair();
        a.send_plain(&hs_c2s(&pk_a));
        b.send_plain(&hs_c2s(&pk_b));

        pump_io!(!a.inbox.is_empty() && !b.inbox.is_empty(), "both get handshake reply");
        assert_eq!(a.inbox[0].len(), 55, "reply = BE 38 + conn(6) + dir(4) + 01 20 + spk(32) + 08 + nonce(8)");
        assert_eq!(&a.inbox[0][0..2], &[0xBE, 0x38], "reply is op-0x38");
        // s2c layout: [0..2]=BE 38, [2..8]=conn, [8..12]=dir, [12..14]=01 20,
        // [14..46]=server pubkey, [46]=08, [47..55]=nonce.
        let parse = |reply: &[u8]| -> ([u8; 32], [u8; 8]) {
            let (mut spk, mut n) = ([0u8; 32], [0u8; 8]);
            spk.copy_from_slice(&reply[14..46]);
            n.copy_from_slice(&reply[47..55]);
            (spk, n)
        };
        let (spk_a, n_a) = parse(&a.inbox[0]);
        a.crypto = Some(CryptoCtx { key: x25519_shared(&sk_a, &spk_a), nonce: n_a });
        let (spk_b, n_b) = parse(&b.inbox[0]);
        b.crypto = Some(CryptoCtx { key: x25519_shared(&sk_b, &spk_b), nonce: n_b });
        assert!(registry.is_active(&a.addr()) && registry.is_active(&b.addr()));
        a.inbox.clear();
        b.inbox.clear();

        // 2.5. Round-start op58 CLOCK-SYNC (capture-proven, s506): the client sends
        //    c2s op58 [clock0=0, token] and BLOCKS until the server replies op58
        //    [server_clock, token] echoing the SAME token back to that client (before
        //    it uploads its loadout). A sends its op58 with a known token; the server
        //    must reply to A ALONE, echoing the token verbatim (decrypted under A's
        //    own key). End-to-end over real ENet + per-peer crypto.
        const TOKEN: i64 = 0x08DECC2E11DD1E98u64 as i64;
        a.send_enc_payload(&crate::arena::combat::messages::clock(0, TOKEN));
        pump_io!(
            a.inbox.iter().any(|m| m.len() > 2
                && m[1] == 0x3a
                && arena_proto::parse_netdata(&m[2..]).props.get(&1)
                    == Some(&arena_proto::NetDataValue::Long(TOKEN))),
            "A receives an op58 clock-sync reply echoing its token (decrypted under A's key)"
        );
        // The clock-sync is point-to-point: B must NOT receive A's reply.
        assert!(
            !b.inbox.iter().any(|m| m.len() > 2 && m[1] == 0x3a),
            "the op58 clock-sync reply goes to the SENDER only, not the opponent"
        );
        a.inbox.clear();
        b.inbox.clear();

        // 3. Both admitted → the tick drives match-start: each client receives the
        //    BackendMatchCreated flow message (decrypted under its OWN key) + the
        //    op50 (MessageType 0x32) net-object spawns for each fighter.
        pump_tick!(
            a.inbox.iter().any(|m| m.ends_with(b"BackendMatchCreated"))
                && b.inbox.iter().any(|m| m.ends_with(b"BackendMatchCreated")),
            "both clients receive BackendMatchCreated from the tick"
        );
        assert!(
            a.inbox.iter().any(|m| m.len() >= 2 && m[1] == 0x32),
            "A receives an op50 net-object spawn"
        );
        assert!(
            a.inbox.iter().all(|m| m.first() == Some(&0xBE)),
            "every tick s2c decrypts to the 0xBE marker (correct per-target key)"
        );

        // 4. A's combat input → the server resolves an authoritative hit → B
        //    receives a ReceiveDamage (carrier 54, NetData propId3=50) with its HP
        //    pool decremented. The A→B authoritative-combat path, end to end.
        //    Combat only resolves once the round is LIVE (the StateTimeout flow
        //    heartbeat) — BackendMatchCreated (step 3) is still the pre-live hold, and
        //    resolve drops inputs off StateTimeout. Wait for the live round first.
        pump_tick!(
            a.inbox.iter().any(|m| m.ends_with(b"StateTimeout")),
            "the round goes live (StateTimeout heartbeat) before combat input resolves"
        );
        a.inbox.clear();
        b.inbox.clear();
        a.send_enc(0x84, 0x36); // carrier 54 = combat-input family
        pump_tick!(
            b.inbox.iter().any(|m| m.len() > 2
                && m[1] == 0x36
                && arena_proto::parse_netdata(&m[2..]).int(3) == Some(50)),
            "B receives a ReceiveDamage from A's input"
        );
        let dmg = b
            .inbox
            .iter()
            .find(|m| m.len() > 2 && m[1] == 0x36 && arena_proto::parse_netdata(&m[2..]).int(3) == Some(50))
            .expect("ReceiveDamage present");
        let packed = match arena_proto::parse_netdata(&dmg[2..]).props.get(&4) {
            Some(arena_proto::NetDataValue::ULong(v)) => *v,
            _ => panic!("ReceiveDamage missing packed stats"),
        };
        // Health is bits 20-29 of the stat word, i.e. bits 52-61 of the full ULong —
        // NOT the low 10 bits of the high half, which is Magicka. [PackedStats]
        let hp = ((packed >> crate::arena::combat::state::PackedStats::HEALTH_SHIFT) & 0x3ff) as u16;
        assert!(hp > 0 && hp < 1023, "B's wire HP is a fraction below full after the swing (got {hp})");

        // 5. A concedes. Drive the complete retail terminal walk, retire the match,
        // and gracefully disconnect both ENet peers exactly as `serve` does.
        a.inbox.clear();
        b.inbox.clear();
        a.send_enc(0x1C, arena_proto::GameMessageId::ConcedeMatch as u8);
        let mut finished = Vec::new();
        for _ in 0..2000 {
            while pump(&mut server, &registry, &mut peer_at) {}
            vnow += Duration::from_millis(250);
            for (addr, channel, bytes) in registry.tick_matches(vnow) {
                send_to(&mut server, &peer_at, &addr, channel, &bytes);
            }
            finished = registry.take_finished_peers();
            server.flush();
            a.drain();
            b.drain();
            if !finished.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(finished.len(), 2, "the first match retires both peers");
        assert_eq!(registry.active_count(), 0, "the first match releases its slot");
        let first_a_addr = a.addr();
        let first_b_addr = b.addr();
        for addr in finished {
            let pid = peer_at
                .remove(&addr)
                .expect("finished peer still has an ENet owner");
            server.peer_mut(pid).disconnect(0);
        }
        server.flush();

        // Let each client observe the graceful disconnect, but deliberately do NOT
        // service the resulting server-side Disconnect events yet. Reconnect first;
        // this is the tight lobby→queue timing that used to let an old generation's
        // delayed Disconnect erase the newly admitted match at the same address.
        for _ in 0..2000 {
            a.drain();
            b.drain();
            if a.disconnected && b.disconnected {
                break;
            }
            server.flush();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(a.disconnected && b.disconnected, "both clients return to the lobby");

        // 6. Allocate a second H2H match and reconnect both client hosts. Their UDP
        // source addresses must be byte-for-byte identical to match 1.
        let second_gsid = Uuid::new_v4();
        assert!(registry.allocate(
            &["psess-a2".to_string(), "psess-b2".to_string()],
            Vec::new(),
            second_gsid,
        ));
        a.reconnect(server_addr);
        b.reconnect(server_addr);
        assert_eq!(a.addr(), first_a_addr, "A reuses its UDP source address");
        assert_eq!(b.addr(), first_b_addr, "B reuses its UDP source address");

        pump_io!(a.connected && b.connected, "both clients reconnect for match 2");

        let (sk_a2, pk_a2) = gen_keypair();
        let (sk_b2, pk_b2) = gen_keypair();
        a.send_plain(&hs_c2s(&pk_a2));
        b.send_plain(&hs_c2s(&pk_b2));
        pump_io!(
            !a.inbox.is_empty() && !b.inbox.is_empty(),
            "both clients get the second handshake reply"
        );
        let (spk_a2, n_a2) = parse(&a.inbox[0]);
        a.crypto = Some(CryptoCtx { key: x25519_shared(&sk_a2, &spk_a2), nonce: n_a2 });
        let (spk_b2, n_b2) = parse(&b.inbox[0]);
        b.crypto = Some(CryptoCtx { key: x25519_shared(&sk_b2, &spk_b2), nonce: n_b2 });
        assert!(registry.is_active(&a.addr()) && registry.is_active(&b.addr()));
        a.inbox.clear();
        b.inbox.clear();

        pump_tick!(
            a.inbox.iter().any(|m| m.ends_with(b"BackendMatchCreated"))
                && b.inbox.iter().any(|m| m.ends_with(b"BackendMatchCreated")),
            "both reused addresses receive BackendMatchCreated for match 2"
        );
        pump_tick!(
            a.inbox.iter().any(|m| m.ends_with(b"StateTimeout"))
                && b.inbox.iter().any(|m| m.ends_with(b"StateTimeout")),
            "the second H2H match reaches a live, damageable round"
        );
    }

    /// CRE-SOAK over the REAL transport: full solo-vs-bot matches, start to finish,
    /// through a rusty_enet server host + a rusty_enet client on loopback UDP, the
    /// registry's X25519 key exchange, ChaCha20 per-peer sealing and the retail channel
    /// map — the whole server-side path a phone's match takes, minus HTTP
    /// matchmaking. Loadouts are the soak's prod-derived and edge fixtures (perks
    /// resolved from the learned map). The client swings and casts its fixture's
    /// abilities whenever the round is live.
    ///
    /// Per match: every received packet decrypts to the 0xBE marker; the op55
    /// MatchState stream reaches `DisconnectingPlayersAfterMatch`; the op49 victory
    /// card (the ~40 KB, many-fragment frame of report #163) is reassembled and
    /// received; the server retires the match and the client sees the graceful
    /// `disconnect_later`; all inside the engine-timer bound.
    #[test]
    fn soak_full_matches_over_real_enet_and_chacha() {
        use crate::arena::combat::engine::soak_tests::{fixture_loadouts, soak_match_bound};
        use crate::arena::combat::state::{AbilityTag, MatchState};

        let fx = fixture_loadouts();
        let step = Duration::from_millis(50);
        let bound = soak_match_bound(step) + Duration::from_secs(10);
        // Player fixture × bot fixture, spread over the prod sample and every edge build.
        let pairs: Vec<(usize, usize)> =
            (0..12).map(|i| ((i * 7) % fx.len(), (30 + i) % fx.len())).collect();
        let mut summary = Vec::new();

        for (k, &(pa, pb)) in pairs.iter().enumerate() {
            let registry = MatchRegistry::new(4);
            assert!(registry.allocate_with_bots(
                &[format!("soak-{k}")],
                vec![fx[pa].1.clone(), fx[pb].1.clone()],
                Uuid::new_v4(),
                1,
            ));
            let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            let server_addr = server_sock.local_addr().unwrap();
            let mut server = Host::new(
                BladesEnetSocket::new(server_sock),
                HostSettings { peer_limit: 4, ..Default::default() },
            )
            .unwrap();
            let mut peer_at = HashMap::new();
            let mut c = Client::connect(server_addr);
            for _ in 0..2000 {
                while pump(&mut server, &registry, &mut peer_at) {}
                server.flush();
                c.drain();
                if c.connected {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(c.connected, "match {k}: ENet connect");
            let (sk, pk) = gen_keypair();
            c.send_plain(&hs_c2s(&pk));
            for _ in 0..2000 {
                while pump(&mut server, &registry, &mut peer_at) {}
                server.flush();
                c.drain();
                if !c.inbox.is_empty() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let reply = c.inbox.first().expect("key exchange reply").clone();
            let (mut spk, mut nonce) = ([0u8; 32], [0u8; 8]);
            spk.copy_from_slice(&reply[14..46]);
            nonce.copy_from_slice(&reply[47..55]);
            c.crypto = Some(CryptoCtx { key: x25519_shared(&sk, &spk), nonce });
            c.inbox.clear();

            let casts: Vec<String> = fx[pa]
                .1
                .abilities
                .iter()
                .filter(|a| a.tag != AbilityTag::Perk)
                .map(|a| a.instance_uuid.clone())
                .collect();
            let t0 = std::time::Instant::now();
            let mut vnow = t0;
            let mut rng: u64 = 0x5eed_0000 + k as u64;
            let mut states: Vec<u8> = Vec::new();
            let (mut victory_card, mut damage, mut retired) = (false, 0usize, false);
            loop {
                while pump(&mut server, &registry, &mut peer_at) {}
                vnow += step;
                for (addr, channel, bytes) in registry.tick_matches(vnow) {
                    send_to(&mut server, &peer_at, &addr, channel, &bytes);
                }
                for addr in registry.take_finished_peers() {
                    if let Some(&pid) = peer_at.get(&addr) {
                        server.peer_mut(pid).disconnect_later(0);
                    }
                    retired = true;
                }
                server.flush();
                c.drain();
                for m in c.inbox.drain(..) {
                    assert_eq!(m.first(), Some(&0xBE), "match {k}: a packet did not decrypt");
                    let nd = arena_proto::parse_netdata(&m[2..]);
                    if m[1] == 0x35 {
                        if let Some(s) = nd.int(5) {
                            if states.last() != Some(&(s as u8)) {
                                states.push(s as u8);
                            }
                        }
                    }
                    if m[1] == 0x36 {
                        match nd.int(3) {
                            Some(49) => victory_card = true,
                            Some(50) => damage += 1,
                            _ => {}
                        }
                    }
                }
                let live = states.last() == Some(&(MatchState::InRound as u8));
                if live && c.connected {
                    rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    match (rng >> 33) % 12 {
                        0 | 1 => c.send_enc(0x84, 0x36),
                        2 if !casts.is_empty() => {
                            let uuid = &casts[((rng >> 40) as usize) % casts.len()];
                            c.send_enc_payload(&crate::arena::combat::messages::request_execute_ability(
                                564, uuid,
                            ));
                        }
                        _ => {}
                    }
                }
                if c.disconnected {
                    break;
                }
                assert!(
                    vnow.duration_since(t0) < bound,
                    "match {k} ({} vs {}) did not finish within {bound:?}; states {states:?}",
                    fx[pa].0,
                    fx[pb].0
                );
            }
            assert!(retired, "match {k}: the registry retired the finished match");
            assert_eq!(registry.active_count(), 0, "match {k}: slot released");
            assert_eq!(
                states.last(),
                Some(&(MatchState::DisconnectingPlayersAfterMatch as u8)),
                "match {k}: {states:?}"
            );
            assert!(victory_card, "match {k}: the op49 victory card never arrived over ENet");
            assert!(damage > 0, "match {k}: no damage frame reached the client");
            summary.push(format!(
                "{} vs {}: {:?} sim, {} damage frames, states {:?}",
                fx[pa].0,
                fx[pb].0,
                vnow.duration_since(t0),
                damage,
                states
            ));
        }
        for s in &summary {
            eprintln!("CRE-SOAK enet: {s}");
        }
    }
}

#[cfg(test)]
mod post_match_disconnect_tests {
    /// The post-match teardown must use `disconnect_later`, never `disconnect`.
    ///
    /// `enet_peer_disconnect` opens with `enet_peer_reset_queues`, discarding every
    /// unacknowledged outgoing command; `enet_peer_disconnect_later` drains the queue
    /// first. With a 41 KB op49 results card leaving as ~30 fragments, the difference
    /// is whether the client can reassemble its own match result — report #163.
    ///
    /// This is a source guard rather than a behavioural test on purpose: the call sits
    /// inside the live socket loop, and the distinction it protects lives in a C
    /// transliteration we do not drive from tests. The needle is built at runtime so
    /// this file's own source cannot satisfy the assertion by containing it literally.
    #[test]
    fn post_match_teardown_drains_before_disconnecting() {
        let src = include_str!("enet_host.rs");
        // Everything before the first test MODULE. Split on the newline-anchored
        // attribute, not the bare one: there is an INDENTED #[cfg(test)] on a field
        // far above the code being guarded, and splitting on that cut the shipped
        // path out of the haystack entirely — the guard then passed by inspecting
        // nothing.
        let code = src.split("\n#[cfg(test)]").next().expect("source has a body");

        // Match on the METHOD CALL, not on a receiver name. The shipped line was
        // `host.peer_mut(pid).disconnect(0)`, which does not contain any particular
        // variable name — an earlier version of this guard keyed on `peer.` and would
        // have passed happily if the original form came back.
        let bad = format!(".{}(0)", "disconnect");
        let good = format!(".{}(0)", "disconnect_later");

        assert!(
            code.contains(&good),
            "the post-match teardown must call disconnect_later so queued fragments drain"
        );
        // `.disconnect(0)` and `.disconnect_later(0)` are distinct strings — the
        // trailing `(0)` means neither contains the other — so this counts bare calls
        // directly, whatever they are called on.
        assert_eq!(
            code.matches(&bad).count(), 0,
            "a bare disconnect() in the shipped path discards unacknowledged packets — \
             it is what left report #163's client waiting for fragments that never came"
        );
    }
}
