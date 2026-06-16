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
        // Server-initiated combat output (flow-control heartbeat, damage, etc.).
        for (addr, bytes) in registry.tick_matches(std::time::Instant::now()) {
            send_to(&mut host, &peer_at, &addr, &bytes);
        }
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
/// peer → the op-0x38 connect handshake (parse the client pubkey →
/// `admit_connection` → reply our pubkey + nonce).
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

    // Unknown peer ⇒ the retail connect handshake (op 0x38, PLAINTEXT; spec §4.1):
    //   BE 38 | conn_id(6) | 00 00 00 00 | 01 20 | client_pubkey(32) [| zero-pad]
    // rusty_enet has reassembled the (fragmented, ~40 KB-padded) message; we read
    // only the leading fields. Bind the connection (FIFO — no psid on the wire) +
    // reply with our pubkey and the session nonce in the same op-0x38 shape;
    // thereafter the connection's traffic is ChaCha20 (the shared ECDH key + nonce).
    const MARKER: u8 = 0xBE;
    const OP_KEYEXCHANGE: u8 = 0x38;
    if data.len() >= 46
        && data[0] == MARKER
        && data[1] == OP_KEYEXCHANGE
        && data[12] == 0x01
        && data[13] == 0x20
    {
        let conn_id = &data[2..8]; // 6-byte per-connection id, echoed back
        let mut client_pub = [0u8; 32];
        client_pub.copy_from_slice(&data[14..46]);
        match registry.admit_connection(addr, &client_pub) {
            Some((server_pk, nonce)) => {
                let mut reply = Vec::with_capacity(55);
                reply.extend_from_slice(&[MARKER, OP_KEYEXCHANGE]);
                reply.extend_from_slice(conn_id);
                reply.extend_from_slice(&[0, 0, 0, 1]); // s2c direction (c2s sends 0)
                reply.extend_from_slice(&[0x01, 0x20]); // pubkey field: tag 0x01, len 32
                reply.extend_from_slice(&server_pk);
                reply.push(0x08); // nonce field: len 8
                reply.extend_from_slice(&nonce);
                send_to(host, peer_at, &addr, &reply);
                info!("arena-enet: {addr} admitted (op-0x38 key exchange)");
            }
            None => debug!("arena-enet: {addr} op-0x38 handshake but no match has a free slot"),
        }
    } else {
        debug!(
            "arena-enet: {addr} {}B from an unknown peer (not an op-0x38 handshake; b0={:#04x})",
            data.len(),
            data.first().copied().unwrap_or(0)
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
    use arena_proto::{CryptoCtx, chacha20_legacy_xor, x25519_shared};
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
        #[allow(dead_code)] // used by the Phase B combat-input tests
        fn send_enc(&mut self, marker: u8, opcode: u8) {
            let c = self.crypto.clone().expect("keyed");
            let mut ud = vec![marker, opcode];
            chacha20_legacy_xor(&mut ud, &c.key, &c.nonce);
            self.send_plain(&ud);
        }
    }

    /// Two rusty_enet clients, a shared 2-player match: both CONNECT, op-0x38
    /// handshake, and once both are admitted the TICK drives match-start — each
    /// client receives `BackendMatchCreated` + a combat-screen per avatar,
    /// correctly encrypted under its OWN key. Proves pairing (shared match) +
    /// tick-driven s2c delivery + per-target crypto end-to-end. (Combat-action
    /// relay returns as real swipe→damage in Phase B.)
    #[test]
    fn two_player_pairing_and_match_start() {
        let _ = env_logger::builder().is_test(true).try_init();

        let registry = MatchRegistry::new(4);
        let gsid = Uuid::new_v4();
        let (psid_a, psid_b) = ("psess-a".to_string(), "psess-b".to_string());
        assert!(registry.allocate(&[psid_a, psid_b], Vec::new(), gsid)); // matchmaker pairing (FIFO-bound below)

        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let mut server =
            Host::new(server_sock, HostSettings { peer_limit: 16, ..Default::default() }).unwrap();
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
        macro_rules! pump_tick {
            ($cond:expr, $msg:expr) => {{
                let mut ok = false;
                for _ in 0..2000 {
                    while pump(&mut server, &registry, &mut peer_at) {}
                    for (addr, bytes) in registry.tick_matches(std::time::Instant::now()) {
                        send_to(&mut server, &peer_at, &addr, &bytes);
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
        fn hs_c2s(pk: &[u8; 32]) -> Vec<u8> {
            let mut m = vec![0xBE, 0x38, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0, 0, 0, 0, 0x01, 0x20];
            m.extend_from_slice(pk);
            m
        }
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

        // 3. Both admitted → the tick drives match-start: each client receives the
        //    BackendMatchCreated flow message (decrypted under its OWN key) + a
        //    combat-screen (MessageType 0x37) for each avatar.
        pump_tick!(
            a.inbox.iter().any(|m| m.ends_with(b"BackendMatchCreated"))
                && b.inbox.iter().any(|m| m.ends_with(b"BackendMatchCreated")),
            "both clients receive BackendMatchCreated from the tick"
        );
        assert!(
            a.inbox.iter().any(|m| m.len() >= 2 && m[1] == 0x37),
            "A receives a combat-screen message for an avatar"
        );
        assert!(
            a.inbox.iter().all(|m| m.first() == Some(&0xBE)),
            "every tick s2c decrypts to the 0xBE marker (correct per-target key)"
        );

        // 4. A's combat input → the server resolves an authoritative hit → B
        //    receives a ReceiveDamage (carrier 54, NetData propId3=50) with its HP
        //    pool decremented. The A→B authoritative-combat path, end to end.
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
        // Health = low 10 bits of the HIGH 32 (stat word); the low 32 is the seq id.
        let hp = ((packed >> 32) & 0x3ff) as u16;
        assert!(hp > 0 && hp < 1023, "B's wire HP is a fraction below full after the swing (got {hp})");
    }
}
