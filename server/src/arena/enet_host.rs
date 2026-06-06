//! Live arena ENet host — the **real-client** path.
//!
//! Transport: **`rusty_enet`** (pure-Rust ENet port). Chosen over the C-backed
//! `tokio-enet` because that crate's socket layer is Linux-only
//! (`socket2::Type::cloexec`) and fails to build on macOS — blocking local dev.
//! `rusty_enet` is cross-platform, transport-agnostic (it drives our own UDP
//! socket), and **inspectable** — so if Blades' ENet header-flag quirk (`0x4000`
//! sentTime vs vanilla `0x8000`, per `arena-protocol-spec.md` §5) bites interop,
//! we can patch the parse. The retail client ships `libenet.so` → standard ENet,
//! so `rusty_enet` should interop.
//!
//! **What this does.** `rusty_enet` owns the ENet protocol (CONNECT/VERIFY,
//! reliability, ACKs, sequencing, fragmentation, ping/timeout). We sit on top:
//!   - On `Connect`, the ENet session is up but there is no crypto yet.
//!   - The first reliable channel-0 packet is the app handshake — client X25519
//!     pubkey(32) ‖ `playerSessionId` (UTF-8). We `registry.admit` it (ECDH +
//!     a fresh nonce, bound to the match the matchmaker pre-allocated) and reply
//!     with server pubkey(32) ‖ nonce(8).
//!   - Every later packet's user-data is `chacha20(marker ‖ opcode ‖ body)`
//!     (counter 0). `registry.handle_live_user_data` decrypts it, drives the
//!     match FSM, and returns encrypted s2c user-data we send back reliably.
//!
//! This is our **own** minimal handshake — the retail connect-phase byte layout
//! (spec §9 Q-PSESS / where `playerSessionId` rides the `0x84` channel-0
//! `Connection.*` messages) is still being captured (#T5). Since we own both
//! ends today, this lets our client + the loopback test play a full match; when
//! the retail framing is pinned, only the handshake parse here changes — the
//! registry/crypto/FSM path is unchanged. It mirrors `udp.rs` (the raw-socket
//! dev reference) but over real ENet instead of hand-rolled frames.
//!
//! Concurrency: `rusty_enet`'s `service()` is synchronous over a non-blocking
//! socket, so the host runs on its **own OS thread** and calls the (sync,
//! `Mutex`-based) [`MatchRegistry`] directly — never an `.await`, and never a
//! lock held across one.

use std::net::UdpSocket;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use log::{debug, error, info, warn};
use rusty_enet::{Event, Host, HostSettings, Packet};

use crate::ServerGlobal;
use crate::arena::match_registry::MatchRegistry;

/// Bind the arena UDP socket and run the ENet host on a dedicated thread.
/// Returns once the socket is bound and the thread is spawned; the service loop
/// then runs for the lifetime of the process.
pub async fn run_enet_host(globals: Arc<ServerGlobal>) -> anyhow::Result<()> {
    let port = globals.arena.config.udp_port;
    let registry = globals.arena.registry.clone();

    let socket = UdpSocket::bind(("0.0.0.0", port))
        .map_err(|e| anyhow::anyhow!("arena-enet: bind udp/{port}: {e}"))?;
    // One ENet peer per connected client: 2 players per match, plus headroom.
    let peer_limit = (registry.max_matches * 2).clamp(2, 256);

    thread::Builder::new()
        .name("arena-enet".into())
        .spawn(move || serve(socket, registry, peer_limit))
        .map_err(|e| anyhow::anyhow!("arena-enet: spawn host thread: {e}"))?;

    info!("arena-enet: live host bound udp/{port} (rusty_enet, peer_limit {peer_limit})");
    Ok(())
}

/// The ENet service loop (own thread): drain queued events each tick, flush
/// outgoing packets, then yield briefly. Runs until the process exits.
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

    loop {
        loop {
            match host.service() {
                Ok(Some(event)) => handle_event(event, &registry),
                Ok(None) => break,
                Err(e) => {
                    warn!("arena-enet: service error: {e}");
                    break;
                }
            }
        }
        host.flush();
        thread::sleep(Duration::from_millis(2));
    }
}

/// Route one ENet event through the match registry. The event's `peer` is the
/// only handle to the connection, so replies are sent on it directly.
fn handle_event(event: Event<UdpSocket>, registry: &MatchRegistry) {
    match event {
        Event::Connect { peer, .. } => {
            info!("arena-enet: peer connected ({:?})", peer.address());
        }
        Event::Disconnect { peer, .. } => {
            if let Some(addr) = peer.address() {
                registry.remove(&addr);
            }
        }
        Event::Receive {
            peer,
            channel_id,
            packet,
        } => {
            let Some(addr) = peer.address() else {
                debug!("arena-enet: receive from a peer with no address; dropping");
                return;
            };
            let data = packet.data();

            if registry.is_active(&addr) {
                // Active match: decrypt user-data → FSM → encrypted s2c replies.
                if let Some(out) = registry.handle_live_user_data(&addr, data) {
                    match out.opcode {
                        Some(op) => {
                            info!("arena-enet: {addr} → GameMessageId {op} [{}]", out.state)
                        }
                        None => debug!(
                            "arena-enet: {addr} frame with no opcode ({} B, marker {:?})",
                            data.len(),
                            out.marker
                        ),
                    }
                    for reply in &out.replies {
                        if let Err(e) = peer.send(channel_id, &Packet::reliable(reply.as_slice())) {
                            warn!("arena-enet: s2c reply to {addr} failed: {e:?}");
                        }
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
                            if let Err(e) = peer.send(channel_id, &Packet::reliable(reply)) {
                                warn!("arena-enet: handshake reply to {addr} failed: {e:?}");
                            } else {
                                info!("arena-enet: {addr} admitted (psess '{psid}')");
                            }
                        }
                        None => debug!("arena-enet: {addr} handshake for unknown psess '{psid}'"),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::match_registry::{MatchRegistry, gen_keypair};
    use arena_proto::{chacha20_legacy_xor, x25519_shared};
    use uuid::Uuid;

    /// One scheduler tick: drain + dispatch server events through the real
    /// `handle_event`, collect client-side Receives, flush both hosts.
    fn tick(
        server: &mut Host<UdpSocket>,
        client: &mut Host<UdpSocket>,
        registry: &MatchRegistry,
        client_connected: &mut bool,
        client_inbox: &mut Vec<Vec<u8>>,
    ) {
        while let Ok(Some(event)) = server.service() {
            handle_event(event, registry);
        }
        server.flush();
        while let Ok(Some(event)) = client.service() {
            match event {
                Event::Connect { .. } => *client_connected = true,
                Event::Receive { packet, .. } => client_inbox.push(packet.data().to_vec()),
                Event::Disconnect { .. } => {}
            }
        }
        client.flush();
        thread::sleep(Duration::from_millis(1));
    }

    /// The full live path over **real ENet** (loopback): a `rusty_enet` client
    /// CONNECTs, does the pubkey‖psess handshake, sends an encrypted
    /// PlayerLoadoutReady (36); the server's FSM replies with encrypted
    /// PlayerWelcome (21) + PlayerSpawnAvatar (22), which the client decrypts.
    /// Exercises `handle_event` + the registry crypto/FSM end-to-end on the wire.
    #[test]
    fn live_enet_handshake_then_match_loop() {
        let _ = env_logger::builder().is_test(true).try_init();

        let registry = MatchRegistry::new(4);
        let psid = "psess-live-1".to_string();
        assert!(registry.allocate(psid.clone(), Uuid::nil())); // matchmaker reserved it

        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let mut server =
            Host::new(server_sock, HostSettings { peer_limit: 16, ..Default::default() }).unwrap();

        let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client_addr = client_sock.local_addr().unwrap();
        let mut client =
            Host::new(client_sock, HostSettings { peer_limit: 1, ..Default::default() }).unwrap();
        let pid = client.connect(server_addr, 2, 0).unwrap().id();

        let mut connected = false;
        let mut inbox: Vec<Vec<u8>> = Vec::new();

        // 1. ENet CONNECT completes.
        for _ in 0..1000 {
            tick(&mut server, &mut client, &registry, &mut connected, &mut inbox);
            if connected {
                break;
            }
        }
        assert!(connected, "ENet connect did not complete");

        // 2. App handshake: client pubkey ‖ playerSessionId → server pubkey ‖ nonce.
        let (client_sk, client_pk) = gen_keypair();
        let mut hs = client_pk.to_vec();
        hs.extend_from_slice(psid.as_bytes());
        client.peer_mut(pid).send(0, &Packet::reliable(hs)).unwrap();

        inbox.clear();
        for _ in 0..1000 {
            tick(&mut server, &mut client, &registry, &mut connected, &mut inbox);
            if !inbox.is_empty() {
                break;
            }
        }
        assert_eq!(inbox.len(), 1, "expected exactly one handshake reply");
        assert_eq!(inbox[0].len(), 40, "reply = server pubkey(32) + nonce(8)");
        assert!(registry.is_active(&client_addr), "server admitted the peer");

        let mut server_pk = [0u8; 32];
        server_pk.copy_from_slice(&inbox[0][..32]);
        let mut nonce = [0u8; 8];
        nonce.copy_from_slice(&inbox[0][32..40]);
        let key = x25519_shared(&client_sk, &server_pk);

        // 3. Encrypted PlayerLoadoutReady (c2s marker 0x84, opcode 36).
        let mut ud = vec![0x84u8, 36];
        chacha20_legacy_xor(&mut ud, &key, &nonce);
        client.peer_mut(pid).send(0, &Packet::reliable(ud)).unwrap();

        inbox.clear();
        for _ in 0..1000 {
            tick(&mut server, &mut client, &registry, &mut connected, &mut inbox);
            if inbox.len() >= 2 {
                break;
            }
        }
        assert!(
            inbox.len() >= 2,
            "expected PlayerWelcome + PlayerSpawnAvatar, got {}",
            inbox.len()
        );

        let mut ops: Vec<u8> = inbox
            .iter()
            .take(2)
            .map(|m| {
                let mut p = m.clone();
                chacha20_legacy_xor(&mut p, &key, &nonce);
                p[1] // user_data[1] = opcode
            })
            .collect();
        ops.sort();
        assert_eq!(
            ops,
            vec![21, 22],
            "s2c = PlayerWelcome(21) + PlayerSpawnAvatar(22)"
        );
    }
}
