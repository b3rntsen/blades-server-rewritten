//! Live arena ENet host — the **real-client** path.
//!
//! Transport: **`rusty_enet`** (pure-Rust ENet port). Chosen over the C-backed
//! `tokio-enet` because that crate's socket layer is Linux-only
//! (`socket2::Type::cloexec`) and fails to build on macOS — blocking local dev.
//! `rusty_enet` is cross-platform, transport-agnostic (it drives our own UDP
//! socket), and inspectable — so if Blades' ENet header-flag quirk (`0x4000`
//! sentTime vs vanilla `0x8000`, per `arena-protocol-spec.md` §5) bites interop,
//! we can patch the parse. The retail client ships `libenet.so` → standard ENet.
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
use rusty_enet::{Event, Host, HostSettings, Packet, PeerID};

use crate::ServerGlobal;
use crate::arena::match_registry::MatchRegistry;

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
        socket,
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

    loop {
        while pump(&mut host, &registry, &mut peer_at) {}
        host.flush();
        thread::sleep(Duration::from_millis(2));
    }
}

/// One ENet event: extract owned data (releasing the borrow on `host`), then route
/// through the registry and send any replies to their target peers. Returns true
/// if an event was handled, false when the queue is drained (or on error).
fn pump(
    host: &mut Host<UdpSocket>,
    registry: &MatchRegistry,
    peer_at: &mut HashMap<std::net::SocketAddr, PeerID>,
) -> bool {
    // Extract everything we need, then drop the event so `host` is free to send.
    let action = match host.service() {
        Ok(Some(Event::Connect { peer, .. })) => Act::Connect(peer.id(), peer.address()),
        Ok(Some(Event::Disconnect { peer, .. })) => Act::Disconnect(peer.address()),
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
        Act::Disconnect(addr) => {
            if let Some(addr) = addr {
                registry.remove(&addr);
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
    Disconnect(Option<std::net::SocketAddr>),
    Receive(PeerID, Option<std::net::SocketAddr>, Vec<u8>),
}

/// Route a received SEND payload: active peer → decrypt + FSM + relay; unknown
/// peer → the app handshake (pubkey ‖ playerSessionId → admit).
fn handle_packet(
    host: &mut Host<UdpSocket>,
    registry: &MatchRegistry,
    peer_at: &HashMap<std::net::SocketAddr, PeerID>,
    addr: std::net::SocketAddr,
    data: &[u8],
) {
    if registry.is_active(&addr) {
        if let Some(out) = registry.handle_live_user_data(&addr, data) {
            match out.opcode {
                Some(op) => info!("arena-enet: {addr} → GameMessageId {op} [{}]", out.state),
                None => debug!(
                    "arena-enet: {addr} frame with no opcode ({} B, marker {:?})",
                    data.len(),
                    out.marker
                ),
            }
            // Deliver each reply to its TARGET peer (may be the opponent).
            for (target_addr, bytes) in &out.replies {
                send_to(host, peer_at, target_addr, bytes);
            }
        }
        return;
    }

    // Unknown peer ⇒ the app handshake: client pubkey(32) ‖ playerSessionId.
    if data.len() >= 33 {
        let mut client_pub = [0u8; 32];
        client_pub.copy_from_slice(&data[..32]);
        match std::str::from_utf8(&data[32..]) {
            Ok(psid) => match registry.admit(addr, psid, &client_pub) {
                Some((server_pk, nonce)) => {
                    let mut reply = server_pk.to_vec();
                    reply.extend_from_slice(&nonce);
                    send_to(host, peer_at, &addr, &reply);
                    info!("arena-enet: {addr} admitted (psess '{psid}')");
                }
                None => debug!("arena-enet: {addr} handshake for unknown/full psess '{psid}'"),
            },
            Err(_) => debug!("arena-enet: {addr} handshake with non-UTF-8 playerSessionId"),
        }
    } else {
        debug!(
            "arena-enet: {addr} {}B from an unknown peer (not a handshake)",
            data.len()
        );
    }
}

/// Send a reliable channel-0 packet to the peer at `addr` (looked up by PeerID).
fn send_to(
    host: &mut Host<UdpSocket>,
    peer_at: &HashMap<std::net::SocketAddr, PeerID>,
    addr: &std::net::SocketAddr,
    bytes: &[u8],
) {
    if let Some(&pid) = peer_at.get(addr) {
        if let Some(peer) = host.get_peer_mut(pid) {
            if let Err(e) = peer.send(0, &Packet::reliable(bytes)) {
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
    use arena_proto::{CryptoCtx, GameMessageId, chacha20_legacy_xor, x25519_shared};
    use std::net::SocketAddr;
    use uuid::Uuid;

    /// A rusty_enet test client: connect, then send/recv reliable channel-0 frames.
    struct Client {
        host: Host<UdpSocket>,
        pid: PeerID,
        connected: bool,
        inbox: Vec<Vec<u8>>,
        crypto: Option<CryptoCtx>,
    }

    impl Client {
        fn connect(server: SocketAddr) -> Self {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut host =
                Host::new(sock, HostSettings { peer_limit: 1, ..Default::default() }).unwrap();
            let pid = host.connect(server, 2, 0).unwrap().id();
            Client { host, pid, connected: false, inbox: Vec::new(), crypto: None }
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
                    Event::Disconnect { .. } => {}
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
        fn send_enc(&mut self, marker: u8, opcode: u8) {
            let c = self.crypto.clone().expect("keyed");
            let mut ud = vec![marker, opcode];
            chacha20_legacy_xor(&mut ud, &c.key, &c.nonce);
            self.send_plain(&ud);
        }
    }

    /// Two rusty_enet clients, a shared 2-player match: both CONNECT, handshake,
    /// both signal PlayerLoadoutReady → both get Welcome + 2×SpawnAvatar; then A
    /// sends a PlayerCommand and **B receives the relayed PlayerStateChange**.
    /// Proves pairing (shared match) + the A→B opponent relay end-to-end.
    #[test]
    fn two_player_pairing_and_relay() {
        let _ = env_logger::builder().is_test(true).try_init();
        use GameMessageId as G;

        let registry = MatchRegistry::new(4);
        let gsid = Uuid::new_v4();
        let (psid_a, psid_b) = ("psess-a".to_string(), "psess-b".to_string());
        assert!(registry.allocate(&[psid_a.clone(), psid_b.clone()], gsid)); // matchmaker pairing

        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let mut server =
            Host::new(server_sock, HostSettings { peer_limit: 16, ..Default::default() }).unwrap();
        let mut peer_at = HashMap::new();

        let mut a = Client::connect(server_addr);
        let mut b = Client::connect(server_addr);

        // Drive server + both clients until a predicate holds, or panic after budget.
        macro_rules! pump_until {
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

        // 1. Both ENet sessions connect.
        pump_until!(a.connected && b.connected, "both clients connect");

        // 2. App handshake (pubkey ‖ psid) → server pubkey ‖ nonce, per client.
        let (sk_a, pk_a) = gen_keypair();
        let mut hs_a = pk_a.to_vec();
        hs_a.extend_from_slice(psid_a.as_bytes());
        a.send_plain(&hs_a);
        let (sk_b, pk_b) = gen_keypair();
        let mut hs_b = pk_b.to_vec();
        hs_b.extend_from_slice(psid_b.as_bytes());
        b.send_plain(&hs_b);

        pump_until!(!a.inbox.is_empty() && !b.inbox.is_empty(), "both get handshake reply");
        assert_eq!(a.inbox[0].len(), 40, "reply = server pubkey(32) + nonce(8)");
        let key_a = {
            let (mut spk, mut n) = ([0u8; 32], [0u8; 8]);
            spk.copy_from_slice(&a.inbox[0][..32]);
            n.copy_from_slice(&a.inbox[0][32..40]);
            a.crypto = Some(CryptoCtx { key: x25519_shared(&sk_a, &spk), nonce: n });
            ()
        };
        let _ = key_a;
        {
            let (mut spk, mut n) = ([0u8; 32], [0u8; 8]);
            spk.copy_from_slice(&b.inbox[0][..32]);
            n.copy_from_slice(&b.inbox[0][32..40]);
            b.crypto = Some(CryptoCtx { key: x25519_shared(&sk_b, &spk), nonce: n });
        }
        assert!(registry.is_active(&a.addr()) && registry.is_active(&b.addr()));
        a.inbox.clear();
        b.inbox.clear();

        // 3. Both PlayerLoadoutReady → match starts → both get Welcome + 2 spawns.
        a.send_enc(0x84, G::PlayerLoadoutReady as u8);
        b.send_enc(0x84, G::PlayerLoadoutReady as u8);
        pump_until!(a.inbox.len() >= 3 && b.inbox.len() >= 3, "both get welcome + 2 spawns");
        let a_ops: Vec<u8> = a.inbox.iter().map(|m| m[1]).collect();
        assert!(a_ops.contains(&(G::PlayerWelcome as u8)), "A welcomed");
        assert_eq!(
            a_ops.iter().filter(|&&o| o == G::PlayerSpawnAvatar as u8).count(),
            2,
            "A sees both avatars spawn"
        );
        a.inbox.clear();
        b.inbox.clear();

        // 4. A sends a command → relayed to B (the core two-player proof).
        a.send_enc(0x84, G::PlayerCommand as u8);
        pump_until!(!b.inbox.is_empty(), "B receives A's relayed action");
        assert_eq!(
            b.inbox[0][1],
            G::PlayerStateChange as u8,
            "B got A's command relayed as a state change"
        );
        assert!(a.inbox.is_empty(), "A's own command is not echoed back to A");
    }
}
