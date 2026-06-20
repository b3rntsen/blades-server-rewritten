//! Fire-and-forget submission of each match's per-peer ChaCha20 key to the
//! capture platform, so OUR-server arena matches become decryptable the same
//! way a Frida-rigged client's submissions are.
//!
//! # Why
//! The capture pipeline (`scripts/arena-decrypt.py`) pairs a captured
//! `arena_udp_frames` ciphertext with a key from `arena_session_keys`. For
//! retail GameLift matches the key is recovered on-device (a Frida hook POSTs it
//! to `/api/arena/submit-key`). For matches against THIS server there is no such
//! hook — but the server already KNOWS the key: it derives the per-peer
//! ChaCha20 key (the X25519 ECDH shared secret) + the 8-byte nonce in the
//! op-0x38 handshake (`MatchRegistry::admit_connection`). So we POST it to the
//! exact same endpoint, tagged with our arena ENet endpoint
//! (`gamelift_ip`/`gamelift_port`, default `10.99.0.1:7777`) so `pcap-ingest`
//! and `arena-decrypt` can find and use it.
//!
//! # Contract (web `POST /api/arena/submit-key`, see web/app/api/arena/submit-key/route.ts)
//!   - `Authorization: Bearer <token>` — a `frida_submit_tokens` row (mints to a
//!     `users.id`); the key is filed against that user's most-recent
//!     `capture_session`.
//!   - JSON body `{ key_b64, nonce_b64, ts, gamelift_ip, gamelift_port }`:
//!       * `key_b64`   — standard base64 of the 32-byte key (required).
//!       * `nonce_b64` — standard base64 of the 8-byte nonce (required; 8 or 12).
//!       * `ts`        — epoch ms (session-window match; we send "now").
//!       * `gamelift_ip` / `gamelift_port` — our arena endpoint.
//!
//! # How it reaches the endpoint
//! The default URL is `http://newblades-web:3000/api/arena/submit-key`: the
//! arena-server container shares the external `edge_net` bridge with the
//! `newblades-web` container (docker-compose.arena.yml + docker-compose.prod.yml)
//! and reaches it by service name — plain HTTP, no TLS, reusing the web
//! handler's auth + session-resolve + idempotent insert. (The on-device gadget
//! instead POSTs plain HTTP to the wg0 forwarder `10.99.0.1:8889` which relays
//! to this same handler; from the container the direct service name is simpler.)
//! Overridable via `ARENA_SUBMIT_URL` for the rare host-network deployment.
//!
//! # Safety
//! Submission is **fire-and-forget**: it runs as a detached task on the captured
//! tokio runtime handle (the ENet host runs on its own OS thread, so it cannot
//! `tokio::spawn` directly — it uses this stored `Handle`), with a hard timeout,
//! and a failed/timed-out/refused submit only logs — it NEVER blocks or panics
//! the match. Gated by `ARENA_SUBMIT_KEYS` (default on); disabled (a no-op) when
//! off, when no token is configured, or when no tokio runtime was available at
//! startup.

use std::time::Duration;

use log::{debug, info, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// Hard ceiling on one submit's connect+write; the capture box is small and may
/// be busy, but the key submit must never tie up a task.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolved configuration for key submission, built once from `ArenaConfig`.
#[derive(Clone, Debug)]
pub struct KeySubmitConfig {
    /// Master on/off (`ARENA_SUBMIT_KEYS`). When false the submitter is a no-op.
    pub enabled: bool,
    /// Full endpoint URL, e.g. `http://newblades-web:3000/api/arena/submit-key`.
    pub url: String,
    /// Bearer token (`frida_submit_tokens.token`) mapping to the contributing
    /// user. Without it, submission is disabled (the endpoint 401s).
    pub token: Option<String>,
    /// Endpoint tag stored on the key so the captured frames can be matched.
    pub gamelift_ip: String,
    pub gamelift_port: u16,
}

impl KeySubmitConfig {
    /// Build from env. Mirrors the defaults the rest of the arena subsystem uses
    /// (`ARENA_UDP_PORT`, the wg0 address) so the tag lines up with what
    /// `pcap-ingest` seeds (`OUR_ARENA_IP`:`OUR_ARENA_PORT`).
    pub fn from_env() -> Self {
        fn flag(key: &str, default: bool) -> bool {
            match std::env::var(key) {
                Ok(v) => !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off" | ""
                ),
                Err(_) => default,
            }
        }
        KeySubmitConfig {
            enabled: flag("ARENA_SUBMIT_KEYS", true),
            url: std::env::var("ARENA_SUBMIT_URL").unwrap_or_else(|_| {
                "http://newblades-web:3000/api/arena/submit-key".to_string()
            }),
            token: std::env::var("ARENA_SUBMIT_TOKEN")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            gamelift_ip: std::env::var("ARENA_SUBMIT_GAMELIFT_IP")
                .unwrap_or_else(|_| "10.99.0.1".to_string()),
            gamelift_port: std::env::var("ARENA_SUBMIT_GAMELIFT_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(7777),
        }
    }

    /// True when submission can actually happen (enabled + a token present).
    fn active(&self) -> bool {
        self.enabled && self.token.is_some()
    }
}

/// Submits per-peer keys off the ENet thread via a stored runtime handle.
pub struct KeySubmitter {
    config: KeySubmitConfig,
    handle: tokio::runtime::Handle,
}

impl KeySubmitter {
    /// Build a submitter, capturing the CURRENT tokio runtime handle (call from
    /// async/runtime context — `ArenaGlobal::start`). Returns `None` (submission
    /// disabled) when the config is inactive or there is no current runtime, so
    /// the caller can store an `Option<Arc<KeySubmitter>>` and skip the cost
    /// entirely when off.
    pub fn from_config(config: KeySubmitConfig) -> Option<Self> {
        if !config.active() {
            if config.enabled && config.token.is_none() {
                warn!(
                    "arena key-submit: ARENA_SUBMIT_KEYS on but ARENA_SUBMIT_TOKEN unset \
                     — our-server match keys will NOT be submitted (set the token to enable)"
                );
            } else {
                info!("arena key-submit: disabled (ARENA_SUBMIT_KEYS off)");
            }
            return None;
        }
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => {
                warn!(
                    "arena key-submit: no tokio runtime at startup — key submission disabled"
                );
                return None;
            }
        };
        info!(
            "arena key-submit: enabled → {} (tag {}:{})",
            config.url, config.gamelift_ip, config.gamelift_port
        );
        Some(KeySubmitter { config, handle })
    }

    /// Fire-and-forget: POST this peer's (key, nonce) to the capture endpoint.
    /// Returns immediately; the actual network I/O runs as a detached task.
    /// Never blocks, never panics — a failure only logs.
    pub fn submit(&self, key: &[u8; 32], nonce: &[u8; 8]) {
        let Some(token) = self.config.token.clone() else {
            return;
        };
        let body = build_submit_body(
            key,
            nonce,
            &self.config.gamelift_ip,
            self.config.gamelift_port,
            now_epoch_ms(),
        );
        let url = self.config.url.clone();
        self.handle.spawn(async move {
            match post_submit(&url, &token, &body).await {
                Ok(status) if (200..300).contains(&status) => {
                    debug!("arena key-submit: POST -> HTTP {status}")
                }
                Ok(status) => warn!("arena key-submit: POST -> HTTP {status} (not stored)"),
                Err(e) => warn!("arena key-submit: POST failed: {e}"),
            }
        });
    }
}

/// Current epoch milliseconds (the `ts` field; the web handler uses it to pick
/// the contributor's capture_session by time window).
fn now_epoch_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Build the exact JSON the web `/api/arena/submit-key` handler expects.
/// Hand-rolled (the body is tiny + fixed-shape) to avoid pulling serde_json into
/// this hot, simple path. Strings here are never attacker-controlled (an IP
/// literal + base64 of our own key/nonce), so no escaping is needed.
pub(crate) fn build_submit_body(
    key: &[u8; 32],
    nonce: &[u8; 8],
    gamelift_ip: &str,
    gamelift_port: u16,
    ts_ms: u128,
) -> String {
    format!(
        "{{\"key_b64\":\"{}\",\"nonce_b64\":\"{}\",\"ts\":{},\"gamelift_ip\":\"{}\",\"gamelift_port\":{}}}",
        b64_encode(key),
        b64_encode(nonce),
        ts_ms,
        gamelift_ip,
        gamelift_port,
    )
}

/// Standard (RFC 4648) base64 with `=` padding — the alphabet the web handler
/// decodes with `Buffer.from(s, "base64")` and re-validates.
pub(crate) fn b64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Split `http://host[:port]/path` into `(host, port, path)`. Only plain `http`
/// is supported (the endpoint is a same-network container or the wg0 forwarder —
/// no TLS); returns `Err` for anything else so a misconfig is loud, not silent.
pub(crate) fn parse_http_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// URLs are supported (got {url:?})"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("missing host in URL {url:?}"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("bad port in URL {url:?}"))?,
        ),
        None => (authority.to_string(), 80),
    };
    Ok((host, port, path.to_string()))
}

/// Build a minimal HTTP/1.1 POST request (headers + body) for the submit. Pure,
/// so it is unit-tested without a socket.
pub(crate) fn build_http_request(host: &str, path: &str, token: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    )
}

/// Connect, write the POST, read just enough of the response to learn the status
/// code, then drop. Bounded by `SUBMIT_TIMEOUT`. Returns the HTTP status code.
async fn post_submit(url: &str, token: &str, body: &str) -> Result<u16, String> {
    let (host, port, path) = parse_http_url(url)?;
    let req = build_http_request(&host, &path, token, body);

    tokio::time::timeout(SUBMIT_TIMEOUT, async {
        let mut stream = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| format!("connect {host}:{port}: {e}"))?;
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| format!("write: {e}"))?;
        stream.flush().await.map_err(|e| format!("flush: {e}"))?;
        // Read the status line only (e.g. "HTTP/1.1 200 OK") — we don't need the
        // body, and `Connection: close` lets the server tear down after replying.
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 128];
        let n = stream.read(&mut buf).await.map_err(|e| format!("read: {e}"))?;
        parse_status_code(&buf[..n])
    })
    .await
    .map_err(|_| format!("timed out after {SUBMIT_TIMEOUT:?}"))?
}

/// Pull the 3-digit status code out of an HTTP/1.x status line.
fn parse_status_code(bytes: &[u8]) -> Result<u16, String> {
    let line = String::from_utf8_lossy(bytes);
    let mut parts = line.split_whitespace();
    let _http = parts.next();
    parts
        .next()
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| format!("no status code in response head {:?}", line.get(..40)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_matches_known_vectors() {
        // RFC 4648 §10 vectors.
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(b64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn b64_lengths_for_key_and_nonce() {
        // 32 bytes -> 44 chars (one pad); 8 bytes -> 12 chars (no pad). The web
        // handler asserts the decoded byte length is exactly 32 / (8 or 12).
        let key = [0xABu8; 32];
        let nonce = [0x11u8; 8];
        let ek = b64_encode(&key);
        let en = b64_encode(&nonce);
        assert_eq!(ek.len(), 44);
        assert_eq!(en.len(), 12);
        // Round-trip the byte count via a reference decoder.
        assert_eq!(b64_decode_len(&ek), 32);
        assert_eq!(b64_decode_len(&en), 8);
    }

    // A minimal standard-base64 length check mirroring the web guard: count the
    // decoded bytes from a canonical (padded) base64 string.
    fn b64_decode_len(s: &str) -> usize {
        let no_pad = s.trim_end_matches('=').len();
        no_pad * 3 / 4
    }

    #[test]
    fn submit_body_is_the_expected_json_shape() {
        let key = [1u8; 32];
        let nonce = [2u8; 8];
        let body = build_submit_body(&key, &nonce, "10.99.0.1", 7777, 1_700_000_000_123);
        // Exact shape the route parses: key_b64, nonce_b64, ts (number),
        // gamelift_ip (string), gamelift_port (number).
        assert_eq!(
            body,
            format!(
                "{{\"key_b64\":\"{}\",\"nonce_b64\":\"{}\",\"ts\":1700000000123,\"gamelift_ip\":\"10.99.0.1\",\"gamelift_port\":7777}}",
                b64_encode(&key),
                b64_encode(&nonce),
            )
        );
        // And it must be valid JSON with the right field types.
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["key_b64"].as_str().unwrap().len(), 44);
        assert_eq!(v["nonce_b64"].as_str().unwrap().len(), 12);
        assert_eq!(v["ts"].as_u64().unwrap(), 1_700_000_000_123);
        assert_eq!(v["gamelift_ip"], "10.99.0.1");
        assert_eq!(v["gamelift_port"], 7777);
    }

    #[test]
    fn parse_url_variants() {
        assert_eq!(
            parse_http_url("http://newblades-web:3000/api/arena/submit-key").unwrap(),
            ("newblades-web".to_string(), 3000, "/api/arena/submit-key".to_string())
        );
        // Default port 80 when omitted.
        assert_eq!(
            parse_http_url("http://10.99.0.1:8889/api/arena/submit-key").unwrap(),
            ("10.99.0.1".to_string(), 8889, "/api/arena/submit-key".to_string())
        );
        assert_eq!(
            parse_http_url("http://host/p").unwrap(),
            ("host".to_string(), 80, "/p".to_string())
        );
        // No path -> "/".
        assert_eq!(
            parse_http_url("http://host:9").unwrap(),
            ("host".to_string(), 9, "/".to_string())
        );
        // HTTPS / garbage rejected (loud misconfig).
        assert!(parse_http_url("https://host/p").is_err());
        assert!(parse_http_url("host:3000/p").is_err());
    }

    #[test]
    fn http_request_has_required_headers() {
        let body = build_submit_body(&[7u8; 32], &[9u8; 8], "10.99.0.1", 7777, 42);
        let req = build_http_request("newblades-web", "/api/arena/submit-key", "tok-123", &body);
        assert!(req.starts_with("POST /api/arena/submit-key HTTP/1.1\r\n"));
        assert!(req.contains("\r\nHost: newblades-web\r\n"));
        assert!(req.contains("\r\nAuthorization: Bearer tok-123\r\n"));
        assert!(req.contains("\r\nContent-Type: application/json\r\n"));
        assert!(req.contains(&format!("\r\nContent-Length: {}\r\n", body.len())));
        assert!(req.contains("\r\nConnection: close\r\n"));
        // Body present after the blank line, intact.
        let (_head, sent_body) = req.split_once("\r\n\r\n").unwrap();
        assert_eq!(sent_body, body);
    }

    #[test]
    fn status_code_parsing() {
        assert_eq!(parse_status_code(b"HTTP/1.1 200 OK\r\n").unwrap(), 200);
        assert_eq!(parse_status_code(b"HTTP/1.1 401 Unauthorized\r\n").unwrap(), 401);
        assert_eq!(parse_status_code(b"HTTP/1.0 500 Internal\r\n").unwrap(), 500);
        assert!(parse_status_code(b"garbage").is_err());
    }

    #[test]
    fn config_flag_parsing() {
        // The env-flag parser used by ARENA_SUBMIT_KEYS: defaults true, and the
        // falsey set turns it off. (Tested via the same matcher logic.)
        fn flag(v: Option<&str>, default: bool) -> bool {
            match v {
                Some(v) => !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off" | ""
                ),
                None => default,
            }
        }
        assert!(flag(None, true));
        assert!(!flag(None, false));
        assert!(!flag(Some("0"), true));
        assert!(!flag(Some("false"), true));
        assert!(!flag(Some("OFF"), true));
        assert!(flag(Some("1"), false));
        assert!(flag(Some("true"), false));
    }
}
