//! **DEBUG / EXPERIMENTAL — arena packet-injection harness.**
//!
//! Two token-gated HTTP routes for firing hand-crafted, correctly-encrypted s2c
//! ENet frames into a LIVE connected peer, so we can discover *experimentally*
//! which packet advances a stuck client from "Connecting…" to "Setting up…".
//!
//!   * `GET  /arena/debug/peers`  — list live matches + peers (pick a target).
//!   * `POST /arena/debug/inject` — enqueue a raw s2c frame (or a builder) for a
//!     match's peer(s); the ENet loop encrypts it under the target's key + sends.
//!
//! # Why this is safe to inject into a live stream
//! Arena UDP crypto is bare ChaCha20 with the counter **reset to 0 per command**
//! (`arena_proto::chacha20_legacy_xor`, spec §4) — there is **no** stateful,
//! incrementing send-nonce. Every frame both directions is encrypted under the
//! peer's *fixed* `(key, nonce)` at counter 0. So an injected frame is encrypted
//! exactly like a normal `tick_matches` reply and **cannot desync** the stream —
//! there is no counter to keep in step. (The `/peers` listing still reports each
//! peer's nonce as its crypto identity.) The actual encrypt+send happens on the
//! ENet thread via `MatchRegistry::drain_debug_injections`, reusing the one true
//! send path; these routes only enqueue.
//!
//! # Auth
//! Gated by `ARENA_DEBUG_TOKEN` (Bearer). To avoid a mandatory env change on the
//! box, it **falls back to `ARENA_IMPORT_TOKEN`** when `ARENA_DEBUG_TOKEN` is
//! unset — both are already configured for our tooling. With neither set the
//! routes are disabled (503).
//!
//! # Disabling it later
//! Remove the two `.service(...)` lines in `main.rs` (and optionally this module
//! + the `debug_*` methods on `MatchRegistry`). Or simply leave both tokens unset
//! — the routes then 503 and the inject queue stays empty (zero hot-path cost: the
//! ENet loop's drain is an empty-vec check per tick).

use std::sync::Arc;

use actix_web::{HttpRequest, HttpResponse, get, http::StatusCode, post, web};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::arena::combat::messages;
use crate::arena::match_registry::DebugTarget;
use crate::{BladeApiError, ServerGlobal};

/// BladeApiError service id for the debug surface (out-of-band, like admin's 9001).
const DEBUG_SERVICE_ID: u64 = 9002;

// --- auth -----------------------------------------------------------------

/// Pull a Bearer token from `Authorization` (or `X-Debug-Token`).
fn extract_token(req: &HttpRequest) -> Option<String> {
    if let Some(v) = req.headers().get("Authorization") {
        if let Ok(v) = v.to_str() {
            if let Some(t) = v.strip_prefix("Bearer ") {
                return Some(t.trim().to_string());
            }
        }
    }
    if let Some(v) = req.headers().get("X-Debug-Token") {
        if let Ok(v) = v.to_str() {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Validate the debug token. The expected value is `ARENA_DEBUG_TOKEN` if set,
/// else `ARENA_IMPORT_TOKEN` (so the harness works on the box with no env change).
/// Unset both ⇒ 503 (disabled); missing header ⇒ 401; mismatch ⇒ 403.
fn check_token(app: &ServerGlobal, req: &HttpRequest) -> Result<(), BladeApiError> {
    let expected = app
        .arena_debug_token
        .as_deref()
        .or(app.arena_import_token.as_deref())
        .filter(|t| !t.is_empty());
    let Some(expected) = expected else {
        return Err(BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, DEBUG_SERVICE_ID, 1));
    };
    match extract_token(req) {
        Some(provided) if provided == expected => Ok(()),
        Some(_) => Err(BladeApiError::new(StatusCode::FORBIDDEN, DEBUG_SERVICE_ID, 2)),
        None => Err(BladeApiError::new(StatusCode::UNAUTHORIZED, DEBUG_SERVICE_ID, 3)),
    }
}

// --- GET /arena/debug/peers ----------------------------------------------

#[derive(Serialize)]
struct PeersResponse {
    matches: Vec<MatchView>,
    /// Total live matches (== matches.len()), for a quick eyeball.
    match_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MatchView {
    game_session_id: Uuid,
    /// Allocation order (FIFO) — also the field you can target as `match_id`.
    order: u64,
    capacity: usize,
    connected: usize,
    /// The match's current flow phase (Connecting / Spawning / BackendMatchCreated
    /// / StateTimeout / …).
    phase: String,
    peers: Vec<PeerView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PeerView {
    slot: usize,
    addr: String,
    player_session_id: String,
    character_name: String,
    /// Hex of the peer's 8-byte ChaCha20 nonce (its crypto identity). NOTE: not a
    /// running counter — the cipher resets counter=0 per command (see module docs).
    nonce_hex: String,
}

#[get("/arena/debug/peers")]
pub async fn debug_peers(
    req: HttpRequest,
    app: web::Data<Arc<ServerGlobal>>,
) -> Result<HttpResponse, BladeApiError> {
    check_token(&app, &req)?;
    let views = app.arena.registry.debug_list();
    let matches: Vec<MatchView> = views
        .into_iter()
        .map(|m| MatchView {
            game_session_id: m.game_session_id,
            order: m.order,
            capacity: m.capacity,
            connected: m.peers.len(),
            phase: m.phase.to_string(),
            peers: m
                .peers
                .into_iter()
                .map(|p| PeerView {
                    slot: p.slot,
                    addr: p.addr.to_string(),
                    player_session_id: p.player_session_id,
                    character_name: p.character_name,
                    nonce_hex: p.nonce_hex,
                })
                .collect(),
        })
        .collect();
    Ok(HttpResponse::Ok().json(PeersResponse {
        match_count: matches.len(),
        matches,
    }))
}

// --- POST /arena/debug/inject --------------------------------------------

/// Inject body. Identify the match by `matchId` (the `gameSessionId` UUID — the
/// `gsid` alias also accepted). `target` = `"0"` | `"1"` | `"both"`. Provide the
/// frame EITHER as `payloadHex` (arbitrary already-formed s2c `user_data`,
/// e.g. an op79) OR as a `builder` (`"matchstatechange"` + `trigger`) so an op79
/// can be fired by trigger string without hand-hexing.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InjectRequest {
    /// `gameSessionId` UUID of the target match. `gsid` is accepted as an alias.
    #[serde(alias = "gsid")]
    match_id: Uuid,
    /// `"0"` | `"1"` | `"both"` (default `"both"`).
    #[serde(default = "default_target")]
    target: String,
    /// Arbitrary decrypted s2c frame as hex (spaces/`0x`/`:` tolerated).
    #[serde(default)]
    payload_hex: Option<String>,
    /// Convenience builder name. Currently: `"matchstatechange"` (op79) — needs
    /// `trigger`; `controllerId` optional (default the captured flow-controller 560).
    #[serde(default)]
    builder: Option<String>,
    /// The op79 `_stateTrigger` string for `builder = "matchstatechange"`.
    #[serde(default)]
    trigger: Option<String>,
    /// Control net-object id for `matchstatechange` (default 560 = our
    /// `MatchCombat::new` flow_controller_id; retail used 119/436).
    #[serde(default)]
    controller_id: Option<i32>,
}

fn default_target() -> String {
    "both".to_string()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InjectResponse {
    game_session_id: Uuid,
    /// How the frame was produced: `"payloadHex"` or `"builder:<name>"`.
    source: String,
    /// The decrypted frame that was injected, as hex (so you can confirm bytes).
    plaintext_hex: String,
    plaintext_len: usize,
    /// One entry per peer the frame was sent to.
    sent: Vec<SentView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SentView {
    slot: usize,
    addr: String,
    nonce_hex: String,
    ciphertext_len: usize,
}

#[post("/arena/debug/inject")]
pub async fn debug_inject(
    req: HttpRequest,
    app: web::Data<Arc<ServerGlobal>>,
    body: web::Json<InjectRequest>,
) -> Result<HttpResponse, BladeApiError> {
    check_token(&app, &req)?;
    let body = body.into_inner();

    let target = parse_target(&body.target)
        .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, DEBUG_SERVICE_ID, 10))?;

    // Build the plaintext frame from a builder or from raw hex (builder wins if both).
    let (plaintext, source) = if let Some(builder) = body.builder.as_deref() {
        build_from_builder(builder, &body)?
    } else if let Some(hex) = body.payload_hex.as_deref() {
        let bytes = decode_hex(hex)
            .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, DEBUG_SERVICE_ID, 11))?;
        if bytes.is_empty() {
            return Err(BladeApiError::new(StatusCode::BAD_REQUEST, DEBUG_SERVICE_ID, 12));
        }
        (bytes, "payloadHex".to_string())
    } else {
        // Neither provided.
        return Err(BladeApiError::new(StatusCode::BAD_REQUEST, DEBUG_SERVICE_ID, 13));
    };

    let plaintext_hex = hex_lower(&plaintext);
    let plaintext_len = plaintext.len();

    // Enqueue; the ENet loop encrypts under the target key + sends next tick.
    // We snapshot the per-peer crypto here only to report nonce/ciphertext len —
    // the SEND uses the same key on the ENet thread (single source of truth).
    let resolved = app
        .arena
        .registry
        .debug_enqueue(body.match_id, target, plaintext.clone());
    let Some(n) = resolved else {
        // No such live match.
        return Err(BladeApiError::new(StatusCode::NOT_FOUND, DEBUG_SERVICE_ID, 14));
    };
    if n == 0 {
        // Match exists but the target slot has no connected peer.
        return Err(BladeApiError::new(StatusCode::NOT_FOUND, DEBUG_SERVICE_ID, 15));
    }

    // Report what WILL be sent (ciphertext len == plaintext len; XOR preserves it).
    // Pull the targeted peers' nonces from the live listing for the response.
    let sent = app
        .arena
        .registry
        .debug_list()
        .into_iter()
        .find(|m| m.game_session_id == body.match_id)
        .map(|m| {
            m.peers
                .into_iter()
                .filter(|p| match target {
                    DebugTarget::Slot(s) => p.slot == s,
                    DebugTarget::Both => true,
                })
                .map(|p| SentView {
                    slot: p.slot,
                    addr: p.addr.to_string(),
                    nonce_hex: p.nonce_hex,
                    ciphertext_len: plaintext_len,
                })
                .collect()
        })
        .unwrap_or_default();

    log::info!(
        "arena DEBUG inject queued: match {} target {:?} ({} B, {}) → {} peer(s)",
        body.match_id, target, plaintext_len, source, n
    );

    Ok(HttpResponse::Ok().json(InjectResponse {
        game_session_id: body.match_id,
        source,
        plaintext_hex,
        plaintext_len,
        sent,
    }))
}

/// `"0"`/`"1"`/`"both"` → [`DebugTarget`] (other small slot ints accepted too).
fn parse_target(s: &str) -> Option<DebugTarget> {
    match s.trim().to_ascii_lowercase().as_str() {
        "both" | "all" | "*" => Some(DebugTarget::Both),
        other => other.parse::<usize>().ok().map(DebugTarget::Slot),
    }
}

/// Build a frame from a named builder. Currently only `matchstatechange` (op79).
fn build_from_builder(
    builder: &str,
    body: &InjectRequest,
) -> Result<(Vec<u8>, String), BladeApiError> {
    match builder.trim().to_ascii_lowercase().as_str() {
        "matchstatechange" | "op79" | "matchstatechangerequest" => {
            let trigger = body.trigger.as_deref().ok_or_else(|| {
                BladeApiError::new(StatusCode::BAD_REQUEST, DEBUG_SERVICE_ID, 20)
            })?;
            let controller = body.controller_id.unwrap_or(560);
            Ok((
                messages::match_state_change_request(controller, trigger),
                format!("builder:matchstatechange(controller={controller},trigger={trigger:?})"),
            ))
        }
        _ => Err(BladeApiError::new(StatusCode::BAD_REQUEST, DEBUG_SERVICE_ID, 21)),
    }
}

/// Decode a hex string, tolerating spaces, `0x` prefixes, and `:`/`,` separators.
/// `None` on an odd nibble count or a non-hex digit.
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':' && *c != ',')
        .collect();
    let cleaned = cleaned.strip_prefix("0x").or_else(|| cleaned.strip_prefix("0X")).unwrap_or(&cleaned);
    if cleaned.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    let bytes = cleaned.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

/// Lowercase-hex a byte slice (response display).
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parsing() {
        assert_eq!(parse_target("0"), Some(DebugTarget::Slot(0)));
        assert_eq!(parse_target("1"), Some(DebugTarget::Slot(1)));
        assert_eq!(parse_target("both"), Some(DebugTarget::Both));
        assert_eq!(parse_target("ALL"), Some(DebugTarget::Both));
        assert_eq!(parse_target(" * "), Some(DebugTarget::Both));
        assert_eq!(parse_target("nope"), None);
    }

    #[test]
    fn hex_decode_tolerant() {
        assert_eq!(decode_hex("be 36 04"), Some(vec![0xBE, 0x36, 0x04]));
        assert_eq!(decode_hex("0xBE3604"), Some(vec![0xBE, 0x36, 0x04]));
        assert_eq!(decode_hex("BE:36:04"), Some(vec![0xBE, 0x36, 0x04]));
        assert_eq!(decode_hex(""), Some(vec![]));
        assert_eq!(decode_hex("abc"), None, "odd nibble count");
        assert_eq!(decode_hex("zz"), None, "non-hex");
    }

    #[test]
    fn hex_roundtrips_lowercase() {
        assert_eq!(hex_lower(&[0xBE, 0x36, 0x0a]), "be360a");
        assert_eq!(decode_hex(&hex_lower(&[0x01, 0xFF, 0x80])), Some(vec![0x01, 0xFF, 0x80]));
    }

    /// The `matchstatechange` builder must produce a byte-exact op79 frame —
    /// identical to `messages::match_state_change_request` (the capture-proven
    /// builder). This is the convenience path for firing trigger strings.
    #[test]
    fn matchstatechange_builder_is_op79() {
        let body = InjectRequest {
            match_id: Uuid::nil(),
            target: "both".to_string(),
            payload_hex: None,
            builder: Some("matchstatechange".to_string()),
            trigger: Some("OpponentShowcase".to_string()),
            controller_id: Some(560),
        };
        let (frame, source) = build_from_builder("matchstatechange", &body).unwrap();
        assert_eq!(frame, messages::match_state_change_request(560, "OpponentShowcase"));
        assert_eq!(&frame[0..2], &[0xBE, 0x36], "op79 rides marker 0xBE + carrier 0x36");
        assert!(frame.ends_with(b"OpponentShowcase"));
        assert!(source.contains("OpponentShowcase"));

        // Missing trigger is a 400, not a panic.
        let bad = InjectRequest {
            trigger: None,
            ..body
        };
        assert!(build_from_builder("matchstatechange", &bad).is_err());
    }

    #[test]
    fn unknown_builder_rejected() {
        let body = InjectRequest {
            match_id: Uuid::nil(),
            target: "both".to_string(),
            payload_hex: None,
            builder: Some("nope".to_string()),
            trigger: None,
            controller_id: None,
        };
        assert!(build_from_builder("nope", &body).is_err());
    }
}
