use std::{net::SocketAddr, sync::Arc, time::Duration};

use actix_web::{
    HttpRequest, HttpResponse, get,
    http::{
        StatusCode,
        header::{HeaderName, HeaderValue},
    },
    post, rt, web,
};
use actix_ws::AggregatedMessage;
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper, insert_into};
use diesel_async::{
    AsyncConnection, AsyncPgConnection, RunQueryDsl, scoped_futures::ScopedFutureExt,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{select, sync::mpsc, time::interval};
use tokio_stream::wrappers::UnboundedReceiverStream;
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal,
    arena::MatchmakingMessage,
    json_db::JsonDbWrapper,
    models::MatchmakingDbEntry,
    schema::matchmaking,
    session::{Session, SessionLookedUpMaybe},
};

const MESSAGE_TYPE_MATCHMAKING: &'static str = "matchmaking";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MatchInfo {
    connect_to: std::net::SocketAddr,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AckInfo {}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WSMatchmakingResponse {
    pub message_type: &'static str,
    pub payload: WSMatchmakingResponseInner,
}

impl WSMatchmakingResponse {
    pub fn new_simple(ticket_status: WSMatchmakingStatus, ticket_id: Uuid) -> Self {
        Self {
            message_type: MESSAGE_TYPE_MATCHMAKING,
            payload: WSMatchmakingResponseInner {
                ticket_id,
                player_session_id: None,
                ticket_status,
                game_session_id: None,
                address: None,
                port: None,
            },
        }
    }
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "PascalCase")]
pub enum WSMatchmakingStatus {
    MatchmakingSearching,
    PotentialMatchCreated,
    #[allow(unused)]
    MatchmakingSucceeded,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WSMatchmakingResponseInner {
    pub ticket_id: Uuid,
    pub player_session_id: Option<String>,
    pub ticket_status: WSMatchmakingStatus,
    pub game_session_id: Option<Uuid>,
    pub address: Option<String>,
    pub port: Option<u16>,
}

/// Assume a transaction is already ongoing
/// Assume current user matchmaking is already locked for update no key if it exist
/// Will crash on failure
/// Will return Some value if another user is found. It will stay locked for this transaction. This user is not deleted (and should be if it had been created)
async fn try_initiate_match(
    conn: &mut AsyncPgConnection,
    user_session: &Session,
    enet_addr: SocketAddr,
) -> Option<Uuid> {
    let matchmaking_entries = matchmaking::table
        .filter(matchmaking::id.ne(user_session.user_id))
        .filter(matchmaking::other_id.is_null())
        .select(MatchmakingDbEntry::as_select())
        .for_update()
        .skip_locked()
        .limit(1)
        .load(conn)
        .await
        .unwrap();

    let entry = if let Some(entry) = matchmaking_entries.get(0) {
        entry
    } else {
        return None;
    };

    diesel::update(matchmaking::table)
        .filter(matchmaking::id.eq(entry.id))
        .set((
            matchmaking::other_id.eq(user_session.user_id),
            matchmaking::match_info.eq(Some(JsonDbWrapper(MatchInfo {
                connect_to: enet_addr,
            }))),
        ))
        .execute(conn)
        .await
        .unwrap();

    return Some(entry.id);
}

async fn send_match_info_to_client(
    entry: &MatchmakingDbEntry,
    session: &mut actix_ws::Session,
    user_id: Uuid,
    ticket_id: Uuid,
) {
    let enet_addr = entry
        .match_info
        .as_ref()
        .expect("match info should be available when a match is started");
    session
        .text(
            serde_json::to_string(&WSMatchmakingResponse {
                message_type: MESSAGE_TYPE_MATCHMAKING,
                payload: WSMatchmakingResponseInner {
                    ticket_id: ticket_id,
                    player_session_id: Some(format!("psess-{}", user_id.to_string())),
                    ticket_status: WSMatchmakingStatus::MatchmakingSucceeded,
                    game_session_id: Some(Uuid::new_v4()),
                    address: Some(enet_addr.0.connect_to.ip().to_string()),
                    port: Some(enet_addr.0.connect_to.port()),
                },
            })
            .unwrap()
            .as_ref(),
        )
        .await
        .unwrap();
}

#[get("/blades.bgs.services/api/rms/v1/public/")]
async fn matchmaking_ws(
    req: HttpRequest,
    stream: web::Payload,
    user_session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
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

    {
        let mut matchmaking_ws = user_session.session.matchmaking_ws.lock().await;
        *matchmaking_ws = Some(tx);
    }

    let user_session_cloned = user_session.session.clone();
    let user_session_cloned_2 = user_session.session.clone();
    let pool_cloned = app_state.db_pool.clone();
    let pool_cloned_2 = app_state.db_pool.clone();

    let enet_addr = app_state.enet_public_addr.clone();

    rt::spawn(async move {
        // spawn another thread to catch panic
        // the basic lifetime of matchmaking
        // 1. If another user is waiting, create match and write match info. Wait for ack, and matchmaking done.
        // 2. Add ourself to the queue
        // (those two first on receiving a matchmaking message)
        // 3. in a loop:
        //   3.1. if someone else has matched against us, send ack and inform client
        //   3.2. if someone else is waiting, create match, write match info and delete ourself. Wait for ack, and matchmaking done.
        //
        // TODO: making something that do not (normally) panic would be nice)
        let thread = rt::spawn(async move {
            struct MatchmakingState {
                ticket_id: Uuid,
            }

            let mut conn = pool_cloned.get().await.unwrap();
            let mut matchmaking_state: Option<MatchmakingState> = None;
            let mut wait_for_ack_from: Option<Uuid> = None;
            // Still wait for client to disconnect
            let mut matchmaking_finished = false;
            let mut matchmaking_interval = interval(Duration::from_secs(1));
            let mut ping_interval = interval(Duration::from_secs(10));
            loop {
                select! {
                    Some(msg) = stream.next() => {
                        match msg {
                            Ok(AggregatedMessage::Text(_text)) => {
                                session.text("Unexpected text message from websocket...").await.unwrap();
                            }

                            Ok(AggregatedMessage::Binary(_bin)) => {
                                session.text("Unexpected binary message from websocket...").await.unwrap();
                            }

                            Ok(AggregatedMessage::Ping(msg)) => {
                                // respond to PING frame with PONG frame
                                session.pong(&msg).await.unwrap();
                            }

                            _ => {}
                        }
                    }
                    _ = matchmaking_interval.tick() => {
                        if let Some(matchmaking_state) = &matchmaking_state && !matchmaking_finished{
                            let user_session_cloned = user_session_cloned.clone();
                            let ticket_id = matchmaking_state.ticket_id.clone();
                            (wait_for_ack_from, session, matchmaking_finished) = conn.transaction(|mut conn| {
                                async move {
                                    if let Some(other_user_id) = wait_for_ack_from {
                                        //TODO: time limit. If it is too long, delete the row (will cancel matchmaking for the other user) and re-add ourself
                                        let matchmaking_entry = matchmaking::table
                                            .filter(matchmaking::id.eq(other_user_id))
                                            .select(MatchmakingDbEntry::as_select())
                                            .for_update()
                                            .load(&mut conn)
                                            .await.unwrap();

                                        if let Some(entry) = matchmaking_entry.get(0) {
                                            if entry.ack_info.is_some() {
                                                send_match_info_to_client(entry, &mut session, user_session_cloned.user_id, ticket_id).await;
                                                matchmaking_finished = true;
                                            }
                                        } else {
                                            todo!("while waiting for ack, entry deleted. Should restore bail out? or restore?")
                                        }
                                    } else {
                                        let matchmaking_entry = matchmaking::table
                                            .filter(matchmaking::id.eq(user_session_cloned.user_id))
                                            .select(MatchmakingDbEntry::as_select())
                                            .for_update()
                                            .load(&mut conn)
                                            .await.unwrap();

                                        let matchmaking_entry = if let Some(matchmaking_entry) = matchmaking_entry.get(0) {
                                            matchmaking_entry
                                        } else {
                                            todo!("Row removed, return error to client and bail out")
                                        };

                                        if let Some(_match_info) = &matchmaking_entry.match_info {
                                            // another user has been matched with us, send ack and inform client
                                            diesel::update(matchmaking::table)
                                                .filter(matchmaking::id.eq(user_session_cloned.user_id))
                                                .set(matchmaking::ack_info.eq(Some(JsonDbWrapper(AckInfo {}))))
                                                .execute(&mut conn)
                                                .await
                                                .unwrap();
                                            send_match_info_to_client(matchmaking_entry, &mut session, user_session_cloned.user_id, ticket_id).await;
                                            // wait a few seconds before deleting our entry, so the other thread can read the ack.
                                            tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                                            matchmaking_finished = true;
                                        } else if let Some(other_user_id) = try_initiate_match(&mut *conn, &*user_session_cloned, enet_addr.clone()).await {
                                            // we have found a match, send ack and inform client
                                            // here, other is the id column of matchmaking
                                            diesel::delete(
                                                    matchmaking::table.filter(
                                                        matchmaking::id.eq(user_session_cloned.user_id),
                                                    ),
                                                )
                                                .execute(conn)
                                                .await
                                                .unwrap();
                                            wait_for_ack_from = Some(other_user_id);
                                            session.text(serde_json::to_string(&WSMatchmakingResponse::new_simple(WSMatchmakingStatus::PotentialMatchCreated, ticket_id)).unwrap().as_ref()).await.unwrap()
                                        }
                                    }
                                    Ok::<_, diesel::result::Error>((wait_for_ack_from, session, matchmaking_finished))
                                }.scope_boxed()
                            }).await.unwrap();
                        }
                    }
                    _ = ping_interval.tick() => {
                        session.ping(b"").await.unwrap();
                    }
                    Some(msg) = rx.next() => {
                        match msg {
                            MatchmakingMessage::InitiateMatchmaking { ticket_id } => {
                                session.text(serde_json::to_string(&WSMatchmakingResponse::new_simple(WSMatchmakingStatus::MatchmakingSearching, ticket_id)).unwrap().as_ref()).await.unwrap();

                                {
                                    let user_session_cloned = user_session_cloned.clone();
                                    (wait_for_ack_from, session) = conn.transaction(|conn| {
                                        async move {
                                            if let Some(other_user_id) =
                                                try_initiate_match(&mut *conn, &*user_session_cloned, enet_addr.clone()).await
                                            {
                                                wait_for_ack_from = Some(other_user_id);
                                                session.text(serde_json::to_string(&WSMatchmakingResponse::new_simple(WSMatchmakingStatus::PotentialMatchCreated, ticket_id)).unwrap().as_ref()).await.unwrap()
                                            }
                                            Ok::<_, diesel::result::Error>((wait_for_ack_from, session))
                                        }
                                        .scope_boxed()
                                    })
                                    .await
                                    .unwrap();
                                }

                                if wait_for_ack_from.is_none() {
                                    // avoid issue if the entry wasn’t correctly deleted previously
                                    diesel::delete(
                                            matchmaking::table.filter(
                                                matchmaking::id.eq(user_session_cloned.user_id),
                                            ),
                                        )
                                        .execute(&mut conn)
                                        .await
                                        .unwrap();

                                    insert_into(matchmaking::table)
                                        .values(MatchmakingDbEntry {
                                            id: user_session_cloned.user_id,
                                            other_id: None,
                                            match_info: None,
                                            ack_info: None,
                                        })
                                        .execute(&mut conn)
                                        .await
                                        .unwrap();
                                }

                                matchmaking_state = Some(MatchmakingState {
                                    ticket_id: ticket_id,
                                });
                            }
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

        let mut matchmaking_ws = user_session_cloned_2.matchmaking_ws.lock().await;
        *matchmaking_ws = None;

        let mut conn = pool_cloned_2.get().await.unwrap();
        diesel::delete(
            matchmaking::table.filter(matchmaking::id.eq(user_session_cloned_2.user_id)),
        )
        .execute(&mut conn)
        .await
        .unwrap();
    });

    // respond immediately with response connected to WS session

    res.headers_mut().append(
        HeaderName::from_static("sec-websocket-protocol"),
        HeaderValue::from_static("json"),
    );
    Ok(res)
}

#[post("/blades.bgs.services/api/matchmaking/v1/public/matches/create")]
pub async fn create_matchmaking_session(
    _request: web::Json<Value>,
    user_session: SessionLookedUpMaybe,
) -> Result<HttpResponse, BladeApiError> {
    let user_session = user_session.get_session_or_error()?;

    let ticket_id = Uuid::new_v4();

    let matchmaking_tx = user_session.session.matchmaking_ws.lock().await;
    if let Some(tx) = matchmaking_tx.as_ref() {
        if tx
            .send(MatchmakingMessage::InitiateMatchmaking { ticket_id })
            .is_err()
        {
            return Err(BladeApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                1,
                1035,
            ));
        }
    } else {
        return Err(BladeApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            1,
            1035,
        ));
    }

    let response = serde_json::json!({
        "match": {
            "ticketId": ticket_id,
            "status": "QUEUED",
            "port": 0
        }
    });

    Ok(HttpResponse::Ok().json(response))
}
