//! Arena UDP endpoint (milestone c): one socket, per-peer match sessions,
//! X25519 handshake, ChaCha20 decode of inbound ENet frames.
//!
//! Built on `arena_proto` (the byte layer proven byte-for-byte against the
//! production Python decoder). The pure decode/encode/ECDH logic is unit-tested
//! without sockets; a loopback integration test exercises the live socket loop.
//!
//! **Dev handshake.** The retail client's connect handshake (how it conveys its
//! X25519 pubkey + the per-context nonce) is still OPEN — see
//! `docs/arena-protocol-spec.md` §4 (Q-NONCE) / §9 (Q-PSESS). Since our server
//! owns both ends (spec §2/§4 "[server impl]"), v1 defines its **own** minimal
//! handshake for our own client + tests, to validate the UDP/ENet/crypto
//! plumbing end-to-end now:
//!   - c2s packet #1 = the client's 32-byte X25519 public key (raw).
//!   - s2c reply      = server pubkey (32) ‖ chosen nonce (8).
//!   - thereafter     = ENet datagrams; SEND_* user-data is ChaCha20(key,nonce).
//! When the retail handshake is decompiled/known, swap this for a `GameLift`
//! handshake — the decode path (`MatchSession`) is unchanged.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use log::{debug, info, warn};
use rand::RngExt;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use arena_proto::enet::ENET_CMD_SEND_RELIABLE;
use arena_proto::{
    CryptoCtx, chacha20_legacy_xor, first_opcode_in_plaintext, reconstruct_plaintext, x25519_public,
    x25519_shared,
};

/// Per-peer match state: the agreed ChaCha20 context for this client.
pub struct MatchSession {
    pub crypto: CryptoCtx,
}

impl MatchSession {
    /// Decode an inbound ENet datagram to the first GameMessageId opcode (if
    /// any). Returns `None` for control-only frames or undecodable data.
    pub fn decode_opcode(&self, datagram: &[u8]) -> Option<u8> {
        let pt = reconstruct_plaintext(
            datagram,
            &self.crypto.key,
            &self.crypto.nonce,
            None, // no fragment resolver in v1 single-datagram decode
            false,
        )?;
        first_opcode_in_plaintext(&pt)
    }
}

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

/// Random 32-byte X25519 secret + its public key. (Any 32 bytes is a valid
/// secret — X25519 clamps internally.)
fn gen_keypair() -> ([u8; 32], [u8; 32]) {
    let sk: [u8; 32] = rand::rng().random();
    let pk = x25519_public(&sk);
    (sk, pk)
}

/// Random 8-byte nonce (ChaCha20 counter stays 0; all per-context variation is
/// in the nonce — spec §4).
fn gen_nonce() -> [u8; 8] {
    rand::rng().random()
}

/// The arena UDP server: one shared socket, a per-peer session table, and a
/// single demux loop.
pub struct UdpServer {
    socket: Arc<UdpSocket>,
    sessions: Mutex<HashMap<SocketAddr, MatchSession>>,
    /// Test observability: decoded `(peer, opcode)` are forwarded here. `None`
    /// in production.
    tap: Option<UnboundedSender<(SocketAddr, u8)>>,
}

impl UdpServer {
    pub async fn bind(addr: &str) -> io::Result<Arc<Self>> {
        Self::bind_with_tap(addr, None).await
    }

    pub async fn bind_with_tap(
        addr: &str,
        tap: Option<UnboundedSender<(SocketAddr, u8)>>,
    ) -> io::Result<Arc<Self>> {
        let socket = Arc::new(UdpSocket::bind(addr).await?);
        Ok(Arc::new(UdpServer {
            socket,
            sessions: Mutex::new(HashMap::new()),
            tap,
        }))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().expect("bound socket has a local addr")
    }

    /// Demux loop: handshake new peers, decode frames from known peers. Runs
    /// until the socket errors fatally. No lock is held across an `.await`.
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

            let known = self.sessions.lock().await.contains_key(&peer);
            if !known && n == 32 {
                // Dev handshake: client's X25519 pubkey.
                let mut client_pub = [0u8; 32];
                client_pub.copy_from_slice(data);
                let (server_sk, server_pk) = gen_keypair();
                let key = x25519_shared(&server_sk, &client_pub);
                let nonce = gen_nonce();
                self.sessions
                    .lock()
                    .await
                    .insert(peer, MatchSession { crypto: CryptoCtx { key, nonce } });
                let mut reply = server_pk.to_vec();
                reply.extend_from_slice(&nonce);
                if let Err(e) = self.socket.send_to(&reply, peer).await {
                    warn!("arena-udp: handshake reply to {peer} failed: {e}");
                }
                info!("arena-udp: handshake completed with {peer}");
                continue;
            }

            // Known peer (or non-handshake first packet): decode.
            let opcode = {
                let sessions = self.sessions.lock().await;
                sessions.get(&peer).and_then(|s| s.decode_opcode(data))
            };
            match opcode {
                Some(op) => {
                    info!("arena-udp: {peer} → GameMessageId {op}");
                    if let Some(tap) = &self.tap {
                        let _ = tap.send((peer, op));
                    }
                }
                None => debug!("arena-udp: {peer} frame with no decodable opcode ({n} B)"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, timeout};

    /// Pure (no socket): two parties ECDH to the same key, client encrypts a
    /// frame, server decodes the opcode. Proves the crypto+ENet+opcode
    /// integration deterministically.
    #[test]
    fn ecdh_then_decode_opcode() {
        let (server_sk, server_pk) = gen_keypair();
        let (client_sk, client_pk) = gen_keypair();
        let server_key = x25519_shared(&server_sk, &client_pk);
        let client_key = x25519_shared(&client_sk, &server_pk);
        assert_eq!(server_key, client_key, "ECDH must agree");

        let nonce = [9u8; 8];
        let client_crypto = CryptoCtx { key: client_key, nonce };
        // marker 0xBE (s2c), opcode 50 (ReceiveDamage), + body.
        let frame = build_send_reliable(2, 7, &client_crypto, &[0xBE, 50, 0x01, 0x02, 0x03]);

        let server = MatchSession {
            crypto: CryptoCtx { key: server_key, nonce },
        };
        assert_eq!(server.decode_opcode(&frame), Some(50));
    }

    /// Live loopback: real UDP sockets, full handshake, then an encrypted frame
    /// the server decodes (observed via the tap).
    #[tokio::test]
    async fn loopback_handshake_and_decode() {
        let (tap_tx, mut tap_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = UdpServer::bind_with_tap("127.0.0.1:0", Some(tap_tx))
            .await
            .unwrap();
        let addr = server.local_addr();
        let srv = server.clone();
        tokio::spawn(async move { srv.run().await });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(addr).await.unwrap();

        // Handshake: send client pubkey, receive server pubkey + nonce.
        let (client_sk, client_pk) = gen_keypair();
        client.send(&client_pk).await.unwrap();
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

        // Send an encrypted SEND_RELIABLE (marker 0xBE, opcode 50).
        let frame = build_send_reliable(0, 1, &crypto, &[0xBE, 50, 0xDE, 0xAD]);
        client.send(&frame).await.unwrap();

        let (_, op) = timeout(Duration::from_secs(2), tap_rx.recv())
            .await
            .expect("decode tap timed out")
            .unwrap();
        assert_eq!(op, 50, "server decoded the frame's opcode");
    }
}
