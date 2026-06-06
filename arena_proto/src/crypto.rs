//! Arena UDP crypto primitives.
//!
//! Confirmed by the capture platform (`docs/arena-udp-protocol.md` §4,
//! `scripts/arena-decrypt.py`):
//!   - **Cipher:** bare ChaCha20 stream, original-Bernstein **8-byte nonce**,
//!     **no** Poly1305 tag, counter starts at 0 *per command*. This is exactly
//!     libsodium `crypto_stream_chacha20_xor` — the RustCrypto `ChaCha20Legacy`
//!     type (NOT the IETF 12-byte `ChaCha20`).
//!   - **Key:** the X25519 ECDH shared secret used **directly** as the 32-byte
//!     ChaCha20 key — no KDF, no hash.

use chacha20::ChaCha20Legacy;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use x25519_dalek::{PublicKey, StaticSecret};

/// XOR `buf` in place with the ChaCha20 keystream (counter=0). Symmetric: the
/// same call both encrypts and decrypts. Mirrors `_chacha20_stream_xor` in
/// `arena-decrypt.py` (which builds a 16-byte pyca nonce of `0u8*8 || nonce8`,
/// i.e. counter=0 || nonce — identical state to ChaCha20Legacy).
pub fn chacha20_legacy_xor(buf: &mut [u8], key: &[u8; 32], nonce8: &[u8; 8]) {
    let mut cipher = ChaCha20Legacy::new_from_slices(key, nonce8)
        .expect("key is 32 bytes and nonce is 8 bytes by type");
    cipher.apply_keystream(buf);
}

/// Allocating convenience: returns a fresh decrypted/encrypted buffer. Mirrors
/// the Python `_chacha20_stream_xor` return-value shape.
pub fn chacha20_legacy(payload: &[u8], key: &[u8; 32], nonce8: &[u8; 8]) -> Vec<u8> {
    let mut out = payload.to_vec();
    chacha20_legacy_xor(&mut out, key, nonce8);
    out
}

/// X25519 ECDH. The 32-byte shared secret IS the ChaCha20 key (no KDF).
/// `secret` is our private scalar; `peer_pub` is the other side's public key.
/// (X25519 clamps the scalar internally, matching libsodium
/// `crypto_scalarmult_curve25519`.)
pub fn x25519_shared(secret: &[u8; 32], peer_pub: &[u8; 32]) -> [u8; 32] {
    let s = StaticSecret::from(*secret);
    let p = PublicKey::from(*peer_pub);
    *s.diffie_hellman(&p).as_bytes()
}

/// Derive the X25519 public key for a private scalar (for the server's per-match
/// keypair / handshake reply).
pub fn x25519_public(secret: &[u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(*secret)).to_bytes()
}

/// Per-client crypto state for one match: the agreed key + the match nonce.
#[derive(Clone)]
pub struct CryptoCtx {
    pub key: [u8; 32],
    pub nonce: [u8; 8],
}

impl CryptoCtx {
    /// Decrypt (or encrypt) one command's user-data field in place.
    pub fn xor(&self, buf: &mut [u8]) {
        chacha20_legacy_xor(buf, &self.key, &self.nonce);
    }
}

/// **Q1 seam.** Where the 8-byte nonce comes from. The inbound decrypt path and
/// the offline replay harness never need this (the nonce is known up front —
/// read from `arena_session_keys.nonce_b64`). The live *outbound* encrypt path
/// is gated on a real impl: until experiment A1 resolves the nonce origin, a
/// live server must refuse to emit rather than ship a wrong-nonce frame.
pub trait NonceSource: Send + Sync {
    /// The nonce to use, or `None` if it can't be determined (Q1 unresolved).
    fn nonce(&self) -> Option<[u8; 8]>;
}

/// Replay/dev: the nonce is known (e.g. captured from Frida).
pub struct FixedNonce(pub [u8; 8]);
impl NonceSource for FixedNonce {
    fn nonce(&self) -> Option<[u8; 8]> {
        Some(self.0)
    }
}

/// Live default until Q1 closes: refuses to provide a nonce, so the send path
/// fails loud instead of corrupting the stream.
pub struct UnresolvedNonce;
impl NonceSource for UnresolvedNonce {
    fn nonce(&self) -> Option<[u8; 8]> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-answer test: ChaCha20 (key=0, nonce=0, counter=0) block 0 is the
    /// canonical RFC 8439 zero vector. For the all-zero input, the legacy
    /// (64-bit-nonce) and IETF state words are identical, so the first 64
    /// keystream bytes match the well-known value. This pins that our
    /// `ChaCha20Legacy` choice + counter-from-0 is the right primitive.
    #[test]
    fn chacha20_zero_vector() {
        let key = [0u8; 32];
        let nonce = [0u8; 8];
        let mut buf = [0u8; 64];
        chacha20_legacy_xor(&mut buf, &key, &nonce);
        let expected = hex::decode(
            "76b8e0ada0f13d90405d6ae55386bd28\
             bdd219b8a08ded1aa836efcc8b770dc7\
             da41597c5157488d7724e03fb8d84a37\
             6a43b8f41518a11cc387b669b2ee6586",
        )
        .unwrap();
        assert_eq!(&buf[..], &expected[..], "ChaCha20Legacy zero-vector mismatch");
    }

    /// XOR is an involution: encrypt then decrypt returns the original.
    #[test]
    fn round_trip() {
        let key = [7u8; 32];
        let nonce = [0x6d, 0x0c, 0xc5, 0xee, 0x29, 0xfd, 0x9c, 0x58]; // a real captured nonce
        let msg = b"\xbe\x35\x09\xff\x03\x70\x07\x77\x75\xa7\x7d\x02hello arena";
        let ct = chacha20_legacy(msg, &key, &nonce);
        assert_ne!(&ct[..], &msg[..]);
        let pt = chacha20_legacy(&ct, &key, &nonce);
        assert_eq!(&pt[..], &msg[..]);
    }

    /// ECDH is symmetric: shared(a_priv, b_pub) == shared(b_priv, a_pub).
    #[test]
    fn x25519_agreement() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        let a_pub = x25519_public(&a);
        let b_pub = x25519_public(&b);
        let ab = x25519_shared(&a, &b_pub);
        let ba = x25519_shared(&b, &a_pub);
        assert_eq!(ab, ba);
        assert_ne!(ab, [0u8; 32]);
    }

    #[test]
    fn nonce_source_seam() {
        assert_eq!(FixedNonce([9u8; 8]).nonce(), Some([9u8; 8]));
        assert_eq!(UnresolvedNonce.nonce(), None);
    }
}
