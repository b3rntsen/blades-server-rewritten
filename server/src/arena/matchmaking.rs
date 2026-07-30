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

use crate::{BladeApiError, arena::MatchmakingMessage, session::SessionLookedUpMaybe};

#[get("/blades.bgs.services/api/rms/v1/public/")]
async fn matchmaking_ws(
    req: HttpRequest,
    stream: web::Payload,
    user_session: SessionLookedUpMaybe,
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

    user_session.session.set_matchmaking_ws(tx.clone()).await;

    // Kept so the teardown below can tell "the slot still holds MY sender" from
    // "a newer socket has taken over". See the compare-and-clear at the end.
    let my_tx = tx;
    let user_session_cloned = user_session.session.clone();
    rt::spawn(async move {
        // spawn another thread to catch panic
        let thread = rt::spawn(async move {
            loop {
                select! {
                    Some(msg) = stream.next() => {
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
