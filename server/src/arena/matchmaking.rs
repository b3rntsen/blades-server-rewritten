use std::time::Duration;

use actix_web::{
    HttpRequest, HttpResponse, get,
    http::header::{HeaderName, HeaderValue},
    rt, web,
};
use actix_ws::AggregatedMessage;
use futures_util::StreamExt;
use tokio::{select, sync::mpsc, time::sleep};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::{
    BladeApiError, ServerGlobal,
    arena::MatchmakingMessage,
    session::SessionLookedUpMaybe,
};

/// Whether an inbound item ends the socket: the client closed it (a Close frame, or
/// the stream simply ended) or the protocol broke.
///
/// This used to end the socket only when a server PING failed — up to ~20 s after the
/// client was gone (prod 2026-10-11: edge-nginx closed Shoyr's socket at 20:43:13, the
/// arena noticed at 20:43:33). For that whole window the dead socket still held the
/// session's matchmaking slot, so a `MatchmakingSucceeded` was "sent" into it and
/// lost. Three of the five no-show matches of 2026-10-01..11 paired a player whose
/// socket had closed 5–11 s earlier (#375).
fn ends_socket(
    item: &Option<Result<AggregatedMessage, actix_ws::ProtocolError>>,
) -> bool {
    matches!(item, None | Some(Err(_)) | Some(Ok(AggregatedMessage::Close(_))))
}

#[get("/blades.bgs.services/api/rms/v1/public/")]
async fn matchmaking_ws(
    req: HttpRequest,
    stream: web::Payload,
    user_session: SessionLookedUpMaybe,
    app_state: web::Data<std::sync::Arc<ServerGlobal>>,
) -> Result<HttpResponse, BladeApiError> {
    let user_session = user_session.get_session_or_error()?;

    let (mut res, mut session, stream) = actix_ws::handle(&req, stream)?;

    let mut stream = stream
        .aggregate_continuations()
        // aggregate continuation frames up to 1MiB
        .max_continuation_size(2_usize.pow(20));

    //TODO: verify it gets auto-disconnected if client is lost
    let (tx, rx) = mpsc::unbounded_channel::<MatchmakingMessage>();
    let mut rx = UnboundedReceiverStream::new(rx);

    // Register the socket — and, under the same lock, hand it a `MatchmakingSucceeded`
    // the client missed (sent while it was between sockets, or into one it had already
    // abandoned) if its match is still waiting for it (#375).
    let registry = &app_state.arena.registry;
    let now = std::time::Instant::now();
    if user_session
        .session
        .replay_pending_match(Some(tx.clone()), |p| {
            crate::arena::matchmaker::match_still_wants(p, registry, now)
        })
        .await
    {
        log::info!(
            "rms: replayed MatchmakingSucceeded (user {}) on a reconnected socket",
            user_session.session.user_id
        );
    }

    // Kept so the teardown below can tell "the slot still holds MY sender" from
    // "a newer socket has taken over". See the compare-and-clear at the end.
    let my_tx = tx;
    let user_session_cloned = user_session.session.clone();
    rt::spawn(async move {
        // spawn another thread to catch panic
        let thread = rt::spawn(async move {
            loop {
                select! {
                    item = stream.next() => {
                        if ends_socket(&item) {
                            if let Some(Ok(AggregatedMessage::Close(reason))) = item {
                                let _ = session.close(reason).await;
                            }
                            break;
                        }
                        let Some(msg) = item else { break };
                        match msg {
                            Ok(AggregatedMessage::Text(_text)) => {
                                // v1: the client doesn't drive matchmaking over
                                // this socket — it's server-push only.
                                log::debug!("rms: ignoring inbound text frame");
                            }

                            Ok(AggregatedMessage::Binary(_bin)) => {
                                log::debug!("rms: ignoring inbound binary frame");
                            }

                            Ok(AggregatedMessage::Ping(msg)) => {
                                // respond to PING frame with PONG frame
                                let _ = session.pong(&msg).await;
                            }

                            _ => {}
                        }
                    }
                    _ = sleep(Duration::from_secs(10)) => {
                        if session.ping(b"").await.is_err() {
                            break;
                        }
                    }
                    Some(msg) = rx.next() => {
                        // Serialize and push the matchmaker's message as a binary
                        // RMS frame (is_text=0, matching the wire capture).
                        if session.binary(msg.to_rms_json()).await.is_err() {
                            break;
                        }
                    }
                    else => {
                        break;
                    }
                }
            }
        });

        match thread.await {
            Ok(_) => {}
            Err(e) => {
                eprintln!("Caught error in websocket thread: {}", e)
            }
        };

        // COMPARE-AND-CLEAR, never a blind clear.
        //
        // The client reconnects this socket repeatedly (roughly once a minute in
        // practice, and always after a game restart). A reconnect registers the
        // NEW sender first, and only then does the OLD socket's loop notice it is
        // dead and run this teardown. A blind `= None` therefore wiped the sender
        // belonging to the healthy, live socket.
        //
        // The damage was permanent and invisible: `create_match` requires this
        // slot to be populated and answers 409-4-1 when it is not, so matchmaking
        // stayed broken for the rest of the session while the WebSocket sat there
        // exchanging ping/pong perfectly. Observed in production 2026-07-30 —
        // matches/create succeeded at 11:40:48 right after the first socket
        // opened, then 409'd at 11:46:54, 11:47:55 and 11:50:18, each time within
        // a minute of another successful 101 upgrade.
        //
        // Only clear the slot if it still holds OUR sender. Tested in
        // session.rs::matchmaking_slot_tests.
        if !user_session_cloned
            .clear_matchmaking_ws_if_owner(&my_tx)
            .await
        {
            log::debug!("rms: socket closed but a newer one owns the slot; leaving it");
        }
    });

    // respond immediately with response connected to WS session

    res.headers_mut().append(
        HeaderName::from_static("sec-websocket-protocol"),
        HeaderValue::from_static("json"),
    );
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #375: a client-side close must end the socket at once, so the session's
    /// matchmaking slot is released before anything else is "sent" into it.
    #[test]
    fn a_closed_socket_ends_at_once() {
        assert!(ends_socket(&None), "stream ended (TCP closed)");
        assert!(ends_socket(&Some(Ok(AggregatedMessage::Close(None)))));
        assert!(ends_socket(&Some(Err(actix_ws::ProtocolError::BadOpCode))));
        assert!(!ends_socket(&Some(Ok(AggregatedMessage::Ping("x".into())))));
        assert!(!ends_socket(&Some(Ok(AggregatedMessage::Text("{}".into())))));
    }
}
