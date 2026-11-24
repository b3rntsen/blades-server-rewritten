//! WebSocket transport for arena matches (Phase 10 WP17, design note WP12).
//!
//! A browser can't open a UDP socket, so it can't speak ENet. This module carries the
//! same match over a WebSocket instead, into the same [`MatchRegistry`]. Native clients
//! (retail and newblades, on ENet) are unaffected. The flag `ARENA_WS_TRANSPORT=1`
//! turns it on; while it is off the route answers 404 and nothing below is ever
//! instantiated.
//!
//! **Framing.** One binary WebSocket message carries one ENet packet's payload,
//! prefixed by the ENet channel it would have travelled on:
//!
//! ```text
//! [channel u8] [payload …]   payload = exactly the bytes ENet carries
//! ```
//!
//! So the op-0x38 key exchange and the ChaCha20 frames are byte-for-byte the ENet ones,
//! and a client shares its crypto code between both transports.
//!
//! **Identity.** The ENet path binds a connection to its match by the matchmaking
//! request's source IP, because the retail key exchange carries no `playerSessionId`.
//! A browser socket is authenticated instead: the upgrade carries the session token,
//! and the session's remembered `MatchmakingSucceeded` names the `playerSessionId`.
//! The key exchange then goes through [`MatchRegistry::admit`], which binds the
//! fighter slot by that id. A stranger can't take someone else's slot.
//!
//! **Addressing.** The registry keys its peers by `SocketAddr`. Each WebSocket peer
//! gets a synthetic address in the IPv6 discard-only prefix `100::/64` (RFC 6666), which
//! no real ENet peer can have. Replies the registry addresses to such a peer are routed
//! here by [`deliver`] instead of to the ENet host; nothing else in the registry or the
//! match FSM knows which transport a player uses, so ENet ↔ WebSocket matches work.
//!
//! **Threading.** WebSocket sessions are async tasks on the actix runtime. They hand
//! their events to the ENet thread over a [`WsBridge`] queue, and the ENet thread
//! drains it in its service loop (`enet_host::drain_ws`), so every registry call keeps
//! happening on that one thread, exactly as before.
//!
//! **Browser auth.** A browser WebSocket can't set an `Authorization` header. It offers
//! two subprotocols, `nb-arena` and `nb-auth.<hex(token)>`; the server selects
//! `nb-arena`. Native clients may send `Authorization` as on every other route.

use std::collections::HashMap;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc as std_mpsc};
use std::time::Duration;

use actix_web::{
    HttpRequest, HttpResponse, get,
    http::header::{HeaderName, HeaderValue},
    rt, web,
};
use actix_ws::{AggregatedMessage, CloseCode, CloseReason};
use futures_util::StreamExt;
use log::{debug, info, warn};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{ServerGlobal, arena::MatchmakingMessage, session::lookup_authorization};

/// ENet channels the retail client opens (ch0–6). A frame naming another is malformed.
pub const CHANNEL_COUNT: u8 = 7;
/// The subprotocol the server selects. A browser refuses the upgrade unless the server
/// selects one of the protocols it offered.
pub const SUBPROTOCOL: &str = "nb-arena";
/// Prefix of the subprotocol that carries the session token, hex-encoded.
pub const AUTH_SUBPROTOCOL_PREFIX: &str = "nb-auth.";
/// Open arena sockets one user may hold at once.
pub const MAX_SOCKETS_PER_USER: usize = 2;
/// A socket that hasn't sent its key exchange by then is closed (as the ENet connect
/// deadline).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest inbound message accepted. Client frames are small; the large ones (the
/// op54 profile, MatchEnd) only travel server → client.
pub const MAX_INBOUND: usize = 64 * 1024;

/// Is the WebSocket transport switched on (`ARENA_WS_TRANSPORT=1|true|on`)?
pub fn enabled_from_env() -> bool {
    std::env::var("ARENA_WS_TRANSPORT")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------------

/// One WebSocket message: the channel byte, then the payload.
pub fn encode_frame(channel: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(channel);
    out.extend_from_slice(payload);
    out
}

/// Split a WebSocket message into `(channel, payload)`. `None` for an empty message,
/// an unknown channel, or an empty payload.
pub fn decode_frame(msg: &[u8]) -> Option<(u8, &[u8])> {
    let (&channel, payload) = msg.split_first()?;
    if channel >= CHANNEL_COUNT || payload.is_empty() {
        return None;
    }
    Some((channel, payload))
}

// ---------------------------------------------------------------------------------
// Synthetic peer addresses
// ---------------------------------------------------------------------------------

static NEXT_PEER: AtomicU64 = AtomicU64::new(1);

/// A fresh, never-reused address for one WebSocket peer, inside `100::/64`.
fn alloc_addr() -> SocketAddr {
    let n = NEXT_PEER.fetch_add(1, Ordering::Relaxed);
    let ip = Ipv6Addr::new(
        0x0100,
        0,
        0,
        0,
        (n >> 48) as u16,
        (n >> 32) as u16,
        (n >> 16) as u16,
        n as u16,
    );
    SocketAddr::V6(SocketAddrV6::new(ip, 1, 0, 0))
}

/// Does `addr` name a WebSocket peer (as opposed to a real ENet peer)?
pub fn is_ws_addr(addr: &SocketAddr) -> bool {
    match addr {
        SocketAddr::V6(v6) => v6.ip().segments()[..4] == [0x0100, 0, 0, 0],
        SocketAddr::V4(_) => false,
    }
}

// ---------------------------------------------------------------------------------
// Outbound routing (registry → socket)
// ---------------------------------------------------------------------------------

/// What the ENet thread hands a socket task.
#[derive(Debug)]
pub enum WsOut {
    /// One framed message (channel byte + payload).
    Frame(Vec<u8>),
    /// Close after everything queued before it has been sent (ENet's
    /// `disconnect_later`), with this `DisconnectReason`.
    Close(u32),
}

type Outbound = Mutex<HashMap<SocketAddr, mpsc::UnboundedSender<WsOut>>>;

/// Live sockets by synthetic address. Global because the registry's send path
/// (`enet_host::send_to`) has no other handle to reach them; the addresses are unique,
/// so independent bridges (tests) never see each other's peers.
fn outbound() -> &'static Outbound {
    static OUT: OnceLock<Outbound> = OnceLock::new();
    OUT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Queue `bytes` for the WebSocket peer at `addr`. `false` when `addr` isn't a live
/// WebSocket peer, so the caller can fall through to its own "no peer" handling.
pub fn deliver(addr: &SocketAddr, channel: u8, bytes: &[u8]) -> bool {
    if !is_ws_addr(addr) {
        return false;
    }
    let Some(tx) = outbound().lock().unwrap().get(addr).cloned() else {
        return false;
    };
    if channel == 4 {
        // The WP12 head-of-line check: a large channel-4 payload shares the TCP stream
        // with channel 0/1. Logged like the ENet line so the two are comparable.
        info!(
            "arena timing: queued channel=4 to={addr} bytes={} transport=ws at_ms={}",
            bytes.len(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        );
    }
    tx.send(WsOut::Frame(encode_frame(channel, bytes))).is_ok()
}

/// Close the socket at `addr` once its queued frames are sent. `false` when it isn't
/// a live WebSocket peer.
pub fn close_later(addr: &SocketAddr, reason: u32) -> bool {
    if !is_ws_addr(addr) {
        return false;
    }
    match outbound().lock().unwrap().remove(addr) {
        Some(tx) => {
            info!("arena-ws: match finished → close peer {addr} after its queue (reason {reason})");
            let _ = tx.send(WsOut::Close(reason));
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------------
// Inbound bridge (socket → ENet thread)
// ---------------------------------------------------------------------------------

/// What a socket task hands the ENet thread.
#[derive(Debug)]
pub enum WsEvent {
    Connect {
        addr: SocketAddr,
        player_session_id: String,
    },
    Receive {
        addr: SocketAddr,
        channel: u8,
        data: Vec<u8>,
    },
    Disconnect {
        addr: SocketAddr,
    },
}

/// The queue between the socket tasks and the thread that owns the registry, plus the
/// per-user socket count.
pub struct WsBridge {
    tx: std_mpsc::Sender<WsEvent>,
    rx: Mutex<std_mpsc::Receiver<WsEvent>>,
    per_user: Mutex<HashMap<Uuid, usize>>,
}

impl WsBridge {
    pub fn new() -> Arc<Self> {
        let (tx, rx) = std_mpsc::channel();
        Arc::new(Self {
            tx,
            rx: Mutex::new(rx),
            per_user: Mutex::new(HashMap::new()),
        })
    }

    /// Everything queued so far (non-blocking). Called by the ENet thread.
    pub fn drain(&self) -> Vec<WsEvent> {
        self.rx.lock().unwrap().try_iter().collect()
    }

    fn push(&self, ev: WsEvent) {
        let _ = self.tx.send(ev);
    }

    /// Reserve one of a user's [`MAX_SOCKETS_PER_USER`] sockets; released on drop.
    pub fn acquire_user_slot(self: &Arc<Self>, user: Uuid) -> Option<UserSlot> {
        let mut per = self.per_user.lock().unwrap();
        let n = per.entry(user).or_insert(0);
        if *n >= MAX_SOCKETS_PER_USER {
            return None;
        }
        *n += 1;
        Some(UserSlot {
            bridge: self.clone(),
            user,
        })
    }
}

/// One held socket slot of a user.
pub struct UserSlot {
    bridge: Arc<WsBridge>,
    user: Uuid,
}

impl Drop for UserSlot {
    fn drop(&mut self) {
        let mut per = self.bridge.per_user.lock().unwrap();
        if let Some(n) = per.get_mut(&self.user) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                per.remove(&self.user);
            }
        }
    }
}

static PRODUCTION: OnceLock<Arc<WsBridge>> = OnceLock::new();

/// Create the production bridge. Called once by `run_enet_host` when the flag is on.
pub fn init_production() -> Arc<WsBridge> {
    PRODUCTION.get_or_init(WsBridge::new).clone()
}

/// The production bridge, if the transport is switched on.
pub fn production_bridge() -> Option<Arc<WsBridge>> {
    PRODUCTION.get().cloned()
}

// ---------------------------------------------------------------------------------
// The route
// ---------------------------------------------------------------------------------

/// The session token from `Authorization`, or from an `nb-auth.<hex>` subprotocol.
fn authorization_of(req: &HttpRequest) -> Option<String> {
    if let Some(v) = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
    {
        return Some(v.to_string());
    }
    req.headers()
        .get_all("Sec-WebSocket-Protocol")
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .find_map(|p| p.strip_prefix(AUTH_SUBPROTOCOL_PREFIX))
        .and_then(hex_decode_utf8)
}

fn hex_decode_utf8(s: &str) -> Option<String> {
    if !s.len().is_multiple_of(2) || s.len() > 1024 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

/// `wss://<host>/blades.bgs.services/api/arena/v1/match` — the browser's arena
/// connection. Same prefix as the matchmaking socket, so the edge that already proxies
/// that WebSocket proxies this one.
#[get("/blades.bgs.services/api/arena/v1/match")]
pub async fn arena_match_ws(
    req: HttpRequest,
    stream: web::Payload,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<HttpResponse, actix_web::Error> {
    let Some(bridge) = production_bridge() else {
        return Ok(HttpResponse::NotFound().finish());
    };
    let Some(authorization) = authorization_of(&req) else {
        return Ok(HttpResponse::Unauthorized().body("no session token"));
    };
    let looked_up = lookup_authorization(&app_state, &authorization).await?;
    let Ok(session) = looked_up.get_session_or_error() else {
        return Ok(HttpResponse::Unauthorized().body("unknown session"));
    };
    // The match this user was handed by matchmaking names their playerSessionId.
    let psid = match session.session.pending_match().map(|p| p.message) {
        Some(MatchmakingMessage::Succeeded {
            player_session_id, ..
        }) => player_session_id,
        _ => return Ok(HttpResponse::Conflict().body("no pending arena match")),
    };
    let user = session.session.user_id;
    let Some(slot) = bridge.acquire_user_slot(user) else {
        warn!("arena-ws: user {user} already holds {MAX_SOCKETS_PER_USER} arena sockets");
        return Ok(HttpResponse::TooManyRequests().finish());
    };
    upgrade(&req, stream, bridge, psid, slot)
}

/// Upgrade the request and run the socket. Split from the route so tests can mount it
/// without a database-backed session.
pub fn upgrade(
    req: &HttpRequest,
    stream: web::Payload,
    bridge: Arc<WsBridge>,
    player_session_id: String,
    slot: UserSlot,
) -> Result<HttpResponse, actix_web::Error> {
    let (mut res, session, stream) = actix_ws::handle(req, stream)?;
    let stream = stream
        .aggregate_continuations()
        .max_continuation_size(MAX_INBOUND);
    rt::spawn(run_socket(bridge, session, stream, player_session_id, slot));
    res.headers_mut().append(
        HeaderName::from_static("sec-websocket-protocol"),
        HeaderValue::from_static(SUBPROTOCOL),
    );
    Ok(res)
}

async fn run_socket(
    bridge: Arc<WsBridge>,
    mut session: actix_ws::Session,
    mut stream: actix_ws::AggregatedMessageStream,
    player_session_id: String,
    _slot: UserSlot,
) {
    let addr = alloc_addr();
    let (tx, mut rx) = mpsc::unbounded_channel::<WsOut>();
    outbound().lock().unwrap().insert(addr, tx);
    info!("arena-ws: peer connected {addr} (psess {player_session_id})");
    bridge.push(WsEvent::Connect {
        addr,
        player_session_id,
    });

    let handshake_deadline = tokio::time::sleep(HANDSHAKE_TIMEOUT);
    tokio::pin!(handshake_deadline);
    let mut spoke = false;
    let mut close_reason: Option<CloseReason> = None;

    loop {
        tokio::select! {
            item = stream.next() => {
                match item {
                    Some(Ok(AggregatedMessage::Binary(bin))) => match decode_frame(&bin) {
                        Some((channel, payload)) => {
                            spoke = true;
                            bridge.push(WsEvent::Receive { addr, channel, data: payload.to_vec() });
                        }
                        None => {
                            warn!("arena-ws: {addr} sent a malformed frame ({} B); closing", bin.len());
                            close_reason = Some(CloseCode::Protocol.into());
                            break;
                        }
                    },
                    Some(Ok(AggregatedMessage::Ping(p))) => {
                        if session.pong(&p).await.is_err() { break; }
                    }
                    Some(Ok(AggregatedMessage::Text(_))) => {
                        close_reason = Some(CloseCode::Unsupported.into());
                        break;
                    }
                    Some(Ok(AggregatedMessage::Pong(_))) => {}
                    Some(Ok(AggregatedMessage::Close(_))) | None => break,
                    Some(Err(e)) => {
                        debug!("arena-ws: {addr} protocol error: {e}");
                        break;
                    }
                }
            }
            out = rx.recv() => match out {
                Some(WsOut::Frame(frame)) => {
                    if session.binary(frame).await.is_err() { break; }
                }
                Some(WsOut::Close(reason)) => {
                    close_reason = Some(CloseReason {
                        code: CloseCode::Normal,
                        description: Some(format!("DisconnectReason {reason}")),
                    });
                    break;
                }
                None => break,
            },
            _ = &mut handshake_deadline, if !spoke => {
                info!("arena-ws: {addr} sent no key exchange within {HANDSHAKE_TIMEOUT:?}; closing");
                close_reason = Some(CloseCode::Policy.into());
                break;
            }
        }
    }

    outbound().lock().unwrap().remove(&addr);
    bridge.push(WsEvent::Disconnect { addr });
    let _ = session.close(close_reason).await;
    info!("arena-ws: peer closed {addr}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_every_channel() {
        for ch in 0..CHANNEL_COUNT {
            for payload in [&[0xBEu8, 0x38][..], &[0u8; 1][..], &vec![7u8; 70_000][..]] {
                let f = encode_frame(ch, payload);
                assert_eq!(f.len(), payload.len() + 1);
                let (c, p) = decode_frame(&f).expect("decodes");
                assert_eq!((c, p), (ch, payload));
            }
        }
    }

    #[test]
    fn malformed_frames_are_rejected() {
        assert_eq!(decode_frame(&[]), None, "empty message");
        assert_eq!(decode_frame(&[0]), None, "channel with no payload");
        assert_eq!(
            decode_frame(&[CHANNEL_COUNT, 1]),
            None,
            "channel 7 doesn't exist"
        );
        assert_eq!(decode_frame(&[0xFF, 1, 2]), None);
    }

    #[test]
    fn synthetic_addresses_never_collide_with_enet_peers() {
        let a = alloc_addr();
        let b = alloc_addr();
        assert_ne!(a, b);
        assert!(is_ws_addr(&a) && is_ws_addr(&b));
        for real in [
            "127.0.0.1:7777",
            "10.99.0.5:5000",
            "[::1]:7777",
            "[2001:db8::1]:7777",
        ] {
            assert!(!is_ws_addr(&real.parse().unwrap()), "{real}");
        }
    }

    #[test]
    fn deliver_ignores_enet_addresses_and_unknown_sockets() {
        assert!(!deliver(&"127.0.0.1:7777".parse().unwrap(), 0, b"x"));
        assert!(!deliver(&alloc_addr(), 0, b"x"), "not registered");
        assert!(!close_later(&alloc_addr(), 0));
    }

    #[test]
    fn deliver_then_close_keeps_order() {
        let addr = alloc_addr();
        let (tx, mut rx) = mpsc::unbounded_channel();
        outbound().lock().unwrap().insert(addr, tx);
        assert!(deliver(&addr, 4, b"profile"));
        assert!(close_later(&addr, 3));
        assert!(
            !deliver(&addr, 0, b"late"),
            "closed sockets take nothing more"
        );
        match rx.try_recv().unwrap() {
            WsOut::Frame(f) => assert_eq!(f, encode_frame(4, b"profile")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(rx.try_recv().unwrap(), WsOut::Close(3)));
    }

    #[test]
    fn auth_token_from_subprotocol() {
        let token = "nb=0a1b2c3d-0000-4000-8000-000000000001|0a1b2c3d-0000-4000-8000-000000000002";
        let hex: String = token.bytes().map(|b| format!("{b:02x}")).collect();
        let req = actix_web::test::TestRequest::default()
            .insert_header((
                "Sec-WebSocket-Protocol",
                format!("{SUBPROTOCOL}, {AUTH_SUBPROTOCOL_PREFIX}{hex}"),
            ))
            .to_http_request();
        assert_eq!(authorization_of(&req).as_deref(), Some(token));
        let none = actix_web::test::TestRequest::default()
            .insert_header(("Sec-WebSocket-Protocol", SUBPROTOCOL))
            .to_http_request();
        assert_eq!(authorization_of(&none), None);
        let bad = actix_web::test::TestRequest::default()
            .insert_header(("Sec-WebSocket-Protocol", "nb-auth.zz"))
            .to_http_request();
        assert_eq!(authorization_of(&bad), None);
    }

    #[test]
    fn user_slots_are_capped_and_released() {
        let bridge = WsBridge::new();
        let u = Uuid::new_v4();
        let a = bridge.acquire_user_slot(u).unwrap();
        let _b = bridge.acquire_user_slot(u).unwrap();
        assert!(bridge.acquire_user_slot(u).is_none());
        assert!(
            bridge.acquire_user_slot(Uuid::new_v4()).is_some(),
            "per user"
        );
        drop(a);
        assert!(bridge.acquire_user_slot(u).is_some());
    }
}
