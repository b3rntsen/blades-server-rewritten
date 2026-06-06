//! Arena UDP endpoint (milestone c + hardening): one socket, a demux loop over
//! a capacity-bounded [`MatchRegistry`], X25519 handshake, ChaCha20 decode.
//!
//! Built on `arena_proto` (the byte layer proven byte-for-byte against the
//! production Python decoder).
//!
//! **Dev handshake.** The retail client's connect handshake (how it conveys its
//! X25519 pubkey + the per-context nonce, and where the `playerSessionId` sits)
//! is still OPEN — see `docs/arena-protocol-spec.md` §4 (Q-NONCE) / §9
//! (Q-PSESS). Since our server owns both ends, v1 defines its **own** minimal
//! handshake (for our own client + tests) so a connecting client is bound to the
//! match the matchmaker pre-allocated:
//!   - c2s packet #1 = client X25519 pubkey(32) ‖ `playerSessionId` (UTF-8).
//!   - the server admits it **iff** the matchmaker pre-allocated that id
//!     (`registry.admit`), then replies server pubkey(32) ‖ nonce(8).
//!   - thereafter = ENet datagrams; SEND_* user-data is ChaCha20(key, nonce).
//! When the retail handshake is known, swap this framing — the registry/decode
//! path is unchanged.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use log::{debug, info, warn};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedSender;

use arena_proto::enet::ENET_CMD_SEND_RELIABLE;
use arena_proto::{CryptoCtx, chacha20_legacy_xor};

use crate::arena::match_registry::MatchRegistry;

/// Build a minimal ENet `SEND_RELIABLE` datagram carrying an encrypted message
/// (`marker ‖ opcode ‖ body`). peerID `0x3000` (no sentTime flag). Used by our
/// client harness + tests; the same wire shape `arena_proto` decodes.
pub fn build_send_reliable(channel: u8, seq: u16, crypto: &CryptoCtx, plain: &[u8]) -> Vec<u8> {
    let mut ud = plain.to_vec();
    chacha20_legacy_xor(&mut ud, &crypto.key, &crypto.nonce);
    let mut f = vec![0x30, 0x00, ENET_CMD_SEND_RELIABLE, channel];
    f.extend_from_slice(&seq.to_be_bytes());
    f.extend_from_slice(&(ud.len() as u16).to_be_bytes());
    f.extend_from_slice(&ud);
    f
}

/// The arena UDP server: one shared socket + a single demux loop over the
/// shared [`MatchRegistry`].
pub struct UdpServer {
    socket: Arc<UdpSocket>,
    registry: Arc<MatchRegistry>,
    /// Test observability: decoded `(peer, opcode)` are forwarded here. `None`
    /// in production.
    tap: Option<UnboundedSender<(SocketAddr, u8)>>,
}

impl UdpServer {
    pub async fn bind(addr: &str, registry: Arc<MatchRegistry>) -> io::Result<Arc<Self>> {
        Self::bind_with_tap(addr, registry, None).await
    }

    pub async fn bind_with_tap(
        addr: &str,
        registry: Arc<MatchRegistry>,
        tap: Option<UnboundedSender<(SocketAddr, u8)>>,
    ) -> io::Result<Arc<Self>> {
        let socket = Arc::new(UdpSocket::bind(addr).await?);
        Ok(Arc::new(UdpServer { socket, registry, tap }))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().expect("bound socket has a local addr")
    }

    /// Demux loop: decode frames from admitted peers; handshake unknown peers
    /// against the registry. No lock is held across an `.await`.
    pub async fn run(self: Arc<Self>) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let (n, peer) = match self.socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    warn!("arena-udp: recv error: {e}");
                    continue;
                }
            };
            let data = &buf[..n];

            if self.registry.is_active(&peer) {
                if let Some(out) = self.registry.handle_inbound(&peer, data) {
                    match out.opcode {
                        Some(op) => {
                            info!("arena-udp: {peer} → GameMessageId {op} [{}]", out.state);
                            if let Some(tap) = &self.tap {
                                let _ = tap.send((peer, op));
                            }
                        }
                        None => debug!("arena-udp: {peer} frame with no decodable opcode ({n} B)"),
                    }
                    for reply in &out.replies {
                        if let Err(e) = self.socket.send_to(reply, peer).await {
                            warn!("arena-udp: s2c reply to {peer} failed: {e}");
                        }
                    }
                }
                continue;
            }

            // Unknown peer ⇒ dev handshake: pubkey(32) ‖ playerSessionId(UTF-8).
            if n >= 33 {
                let mut client_pub = [0u8; 32];
                client_pub.copy_from_slice(&data[..32]);
                let Ok(psid) = std::str::from_utf8(&data[32..]) else {
                    debug!("arena-udp: {peer} handshake with non-UTF-8 playerSessionId");
                    continue;
                };
                match self.registry.admit(peer, psid, &client_pub) {
                    Some((server_pk, nonce)) => {
                        let mut reply = server_pk.to_vec();
                        reply.extend_from_slice(&nonce);
                        if let Err(e) = self.socket.send_to(&reply, peer).await {
                            warn!("arena-udp: handshake reply to {peer} failed: {e}");
                        }
                    }
                    None => debug!("arena-udp: {peer} handshake for unknown psess '{psid}' — ignored"),
                }
            } else {
                debug!("arena-udp: {peer} {n}B from unknown peer (not a handshake)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::match_registry::{MatchRegistry, gen_keypair};
    use arena_proto::x25519_shared;
    use tokio::time::{Duration, timeout};
    use uuid::Uuid;

    /// allocate → admit → decode, no sockets. Proves the matchmaker→UDP linkage
    /// + ECDH agreement + opcode decode in one path.
    #[test]
    fn allocate_admit_decode() {
        let reg = MatchRegistry::new(2);
        let psid = "psess-test-1".to_string();
        assert!(reg.allocate(psid.clone(), Uuid::nil()));

        let (client_sk, client_pk) = gen_keypair();
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let (server_pk, nonce) = reg.admit(peer, &psid, &client_pk).expect("admit");

        let crypto = CryptoCtx {
            key: x25519_shared(&client_sk, &server_pk),
            nonce,
        };
        let frame = build_send_reliable(0, 1, &crypto, &[0xBE, 50, 0x01, 0x02]);
        let out = reg.handle_inbound(&peer, &frame).expect("active match");
        assert_eq!(out.opcode, Some(50));
        assert!(
            out.replies.is_empty(),
            "ReceiveDamage in Connecting yields no reply"
        );
        assert!(reg.is_active(&peer));
    }

    /// The Semaphore cap rejects over-capacity allocation and frees on removal.
    #[test]
    fn cap_enforced_and_released() {
        let reg = MatchRegistry::new(1);
        assert!(reg.allocate("a".into(), Uuid::nil()));
        assert!(!reg.allocate("b".into(), Uuid::nil()), "second exceeds cap");

        let peer: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        reg.admit(peer, "a", &[1u8; 32]).expect("admit a");
        assert_eq!(reg.available_permits(), 0);

        reg.remove(&peer); // disconnect frees the slot
        assert_eq!(reg.available_permits(), 1);
        assert!(reg.allocate("c".into(), Uuid::nil()), "slot freed");
    }

    /// Live loopback: real sockets, full allocate→handshake→encrypted frame the
    /// server decodes (observed via the tap).
    #[tokio::test]
    async fn loopback_admit_and_decode() {
        let reg = MatchRegistry::new(4);
        let psid = "psess-loop-1".to_string();
        assert!(reg.allocate(psid.clone(), Uuid::nil())); // simulate the matchmaker

        let (tap_tx, mut tap_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = UdpServer::bind_with_tap("127.0.0.1:0", reg.clone(), Some(tap_tx))
            .await
            .unwrap();
        let addr = server.local_addr();
        tokio::spawn({
            let s = server.clone();
            async move { s.run().await }
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(addr).await.unwrap();

        // Handshake: client pubkey ‖ playerSessionId.
        let (client_sk, client_pk) = gen_keypair();
        let mut hs = client_pk.to_vec();
        hs.extend_from_slice(psid.as_bytes());
        client.send(&hs).await.unwrap();

        let mut buf = [0u8; 64];
        let n = timeout(Duration::from_secs(2), client.recv(&mut buf))
            .await
            .expect("handshake reply timed out")
            .unwrap();
        assert_eq!(n, 40, "reply = server pubkey(32) + nonce(8)");
        let mut server_pk = [0u8; 32];
        server_pk.copy_from_slice(&buf[..32]);
        let mut nonce = [0u8; 8];
        nonce.copy_from_slice(&buf[32..40]);
        let crypto = CryptoCtx {
            key: x25519_shared(&client_sk, &server_pk),
            nonce,
        };

        client
            .send(&build_send_reliable(0, 1, &crypto, &[0xBE, 50, 0xDE, 0xAD]))
            .await
            .unwrap();

        let (_, op) = timeout(Duration::from_secs(2), tap_rx.recv())
            .await
            .expect("decode tap timed out")
            .unwrap();
        assert_eq!(op, 50);
    }

    /// A handshake for a playerSessionId the matchmaker never allocated gets no
    /// reply (rejected).
    #[tokio::test]
    async fn unknown_psess_rejected() {
        let reg = MatchRegistry::new(4);
        let server = UdpServer::bind("127.0.0.1:0", reg.clone()).await.unwrap();
        let addr = server.local_addr();
        tokio::spawn({
            let s = server.clone();
            async move { s.run().await }
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(addr).await.unwrap();
        let (_, client_pk) = gen_keypair();
        let mut hs = client_pk.to_vec();
        hs.extend_from_slice(b"psess-never-allocated");
        client.send(&hs).await.unwrap();

        let mut buf = [0u8; 64];
        let r = timeout(Duration::from_millis(400), client.recv(&mut buf)).await;
        assert!(r.is_err(), "server must not reply to an unallocated psess");
    }

    /// Milestone (d): a scripted client connects, sends PlayerLoadoutReady (36);
    /// the server transitions Connecting→InProgress and emits PlayerWelcome (21)
    /// + PlayerSpawnAvatar (22), which the client decodes.
    #[tokio::test]
    async fn match_loop_loadout_then_welcome() {
        let reg = MatchRegistry::new(4);
        let psid = "psess-d-1".to_string();
        assert!(reg.allocate(psid.clone(), Uuid::nil()));
        let server = UdpServer::bind("127.0.0.1:0", reg.clone()).await.unwrap();
        let addr = server.local_addr();
        tokio::spawn({
            let s = server.clone();
            async move { s.run().await }
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(addr).await.unwrap();
        let (client_sk, client_pk) = gen_keypair();
        let mut hs = client_pk.to_vec();
        hs.extend_from_slice(psid.as_bytes());
        client.send(&hs).await.unwrap();

        let mut buf = [0u8; 256];
        let n = timeout(Duration::from_secs(2), client.recv(&mut buf))
            .await
            .expect("handshake reply timed out")
            .unwrap();
        assert_eq!(n, 40);
        let mut server_pk = [0u8; 32];
        server_pk.copy_from_slice(&buf[..32]);
        let mut nonce = [0u8; 8];
        nonce.copy_from_slice(&buf[32..40]);
        let crypto = CryptoCtx {
            key: x25519_shared(&client_sk, &server_pk),
            nonce,
        };

        // c2s PlayerLoadoutReady (36); c2s marker 0x84.
        client
            .send(&build_send_reliable(0, 1, &crypto, &[0x84, 36]))
            .await
            .unwrap();

        let mut got = Vec::new();
        for _ in 0..2 {
            let n = timeout(Duration::from_secs(2), client.recv(&mut buf))
                .await
                .expect("s2c reply timed out")
                .unwrap();
            let pt = arena_proto::reconstruct_plaintext(
                &buf[..n],
                &crypto.key,
                &crypto.nonce,
                None,
                false,
            )
            .expect("decode s2c");
            got.push(arena_proto::first_opcode_in_plaintext(&pt));
        }
        got.sort();
        assert_eq!(
            got,
            vec![Some(21), Some(22)],
            "PlayerWelcome + PlayerSpawnAvatar"
        );
    }
}
