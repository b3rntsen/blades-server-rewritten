use std::{
    fs::File,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use actix_files::Files;
use actix_web::{
    App, HttpServer,
    dev::Service,
    http::header::{HeaderName, HeaderValue},
    main,
    web::Data,
};
use anyhow::{Context, Result, bail};
use bb8::Pool;
use blades_lib::game_data::GameData;
use clap::{Parser, Subcommand};
use diesel_async::{AsyncPgConnection, pooled_connection::AsyncDieselConnectionManager};
use log::debug;
use tokio::select;

mod abyss;
mod admin;
mod analytics;
mod analytics_events;
mod announcements;
mod arena;
mod authentification;
mod challenge;
mod character;
mod character_data;
mod craft;
mod daily_reward;
mod dungeon;
mod dungeon_update;
mod error;
mod gameevent;
mod global_gift;
mod global_shop;
mod inventory;
mod json_db;
pub mod models;
mod quest;
pub mod schema;
mod session;
mod status;
mod town;
mod util;
mod wallet;

pub use error::BladeApiError;
use uuid::Uuid;

use crate::{
    arena::enet_channel::run_enet_channel,
    session::{SessionLookedUpMaybe, SessionStore},
};

#[derive(Parser)]
#[command(name = "blade")]
#[command(about = "Blade server", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the server
    Run {
        /// Database connection string
        #[arg(short, long)]
        connection_string: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        port: u16,
        #[arg(long)]
        static_data: PathBuf,
        #[arg(long)]
        enet_listen_addr: SocketAddr,
        #[arg(long)]
        enet_public_addr: SocketAddr,
    },
}

type DbPool = Pool<AsyncDieselConnectionManager<AsyncPgConnection>>;

pub struct ServerGlobal {
    pub db_pool: DbPool,
    pub session_store: SessionStore,
    pub static_data_path: PathBuf,
    pub game_data: GameData,
    pub enet_listen_addr: SocketAddr,
    //TODO: check the protocol handle IPv6 too.
    pub enet_public_addr: SocketAddr,
    /// Static dev token gating the admin endpoints (see the [`admin`] module).
    /// Read from `ARENA_IMPORT_TOKEN` at startup; `None` (unset) disables the
    /// admin surface entirely (every admin handler returns 503).
    pub arena_import_token: Option<String>,
}

#[main]
async fn main() -> Result<()> {
    env_logger::init();
    debug!("logger initialised");

    let cli = Cli::parse();

    match &cli.command {
        Commands::Run {
            connection_string,
            host,
            port,
            static_data,
            enet_listen_addr,
            enet_public_addr,
        } => {
            let db_pool = Pool::builder()
                .build(AsyncDieselConnectionManager::<AsyncPgConnection>::new(
                    connection_string,
                ))
                .await
                .unwrap();

            let game_data: GameData = {
                let parsed_data_path = static_data.join("parsed.json");
                let mut game_data_file = File::open(&parsed_data_path).unwrap();
                serde_json::from_reader(&mut game_data_file).unwrap()
            };

            // Admin endpoints (see the `admin` module) are gated on this dev
            // token; leaving ARENA_IMPORT_TOKEN unset disables them.
            let arena_import_token = std::env::var("ARENA_IMPORT_TOKEN").ok();

            let server_global = Arc::new(ServerGlobal {
                db_pool,
                session_store: SessionStore::new(Duration::from_hours(24)),
                static_data_path: static_data.clone(),
                game_data,
                enet_listen_addr: enet_listen_addr.clone(),
                enet_public_addr: enet_public_addr.clone(),
                arena_import_token,
            });

            let static_data_clone = static_data.clone();

            let enet_future = tokio::spawn(run_enet_channel(server_global.clone()));

            let srv_future = HttpServer::new(move || {
                App::new()
                    .app_data(Data::new(server_global.clone()))
                    .wrap_fn(|mut req, srv| {
                        let start_timestamp = SystemTime::now();
                        let is_from_blades_api =
                            req.uri().path().starts_with("/blades.bgs.services/");
                        let session_fut = req.extract::<SessionLookedUpMaybe>();
                        let res_fut = srv.call(req);
                        async move {
                            let maybe_session = session_fut.await?;
                            let request_index =
                                maybe_session.get_session_or_error().ok().map(|session| {
                                    session
                                        .session
                                        .request_count
                                        .fetch_add(1, Ordering::Relaxed)
                                });
                            let mut res = res_fut.await?;
                            if is_from_blades_api {
                                res.headers_mut().insert(
                                    HeaderName::from_static("server-request-timestamp"),
                                    HeaderValue::from_str(&format!(
                                        "{}",
                                        start_timestamp
                                            .duration_since(UNIX_EPOCH)
                                            .map(|x| x.as_millis())
                                            .unwrap_or(0)
                                    ))
                                    .unwrap(),
                                );
                                res.headers_mut().insert(
                                    HeaderName::from_static("server-timestamp"),
                                    HeaderValue::from_str(&format!(
                                        "{}",
                                        SystemTime::now()
                                            .duration_since(UNIX_EPOCH)
                                            .map(|x| x.as_millis())
                                            .unwrap_or(0)
                                    ))
                                    .unwrap(),
                                );
                                res.headers_mut().insert(
                                    HeaderName::from_static("server-operation-id"),
                                    HeaderValue::from_str(&Uuid::new_v4().to_string()).unwrap(),
                                );
                                if let Some(request_index) = request_index {
                                    res.headers_mut().insert(
                                        HeaderName::from_static("request-index"),
                                        HeaderValue::from_str(&request_index.to_string()).unwrap(),
                                    );
                                }
                            }
                            Ok(res)
                        }
                    })
                    .service(analytics::blades_bgs_event_analytics)
                    .service(analytics::blades_bgs_stat_analytics)
                    .service(analytics::swrve_batch_submit)
                    .service(analytics::swrve_submit_device_info)
                    .service(analytics::appcenter_log)
                    .service(analytics::swrve_identity_identify)
                    .service(status::check_status)
                    .service(session::sync)
                    .service(authentification::anon_log_in)
                    .service(character::list_characters)
                    .service(character::create_characters)
                    .service(character::get_character)
                    .service(wallet::get_wallet)
                    .service(inventory::get_inventory)
                    .service(analytics_events::list_events)
                    .service(dungeon::get_dungeons)
                    .service(dungeon::enter_quest_dungeon)
                    .service(dungeon_update::dungeon_update)
                    .service(abyss::get_abyss)
                    .service(town::get_town)
                    .service(craft::get_crafts)
                    .service(challenge::get_challenges)
                    .service(gameevent::get_game_events)
                    .service(quest::get_quests)
                    .service(quest::accept_quest)
                    .service(global_shop::get_override)
                    .service(global_shop::get_global_shop_for_character)
                    .service(global_shop::get_iap)
                    .service(global_gift::get_global_gifts)
                    .service(character_data::update_data)
                    .service(daily_reward::get_daily_reward)
                    .service(announcements::get_announcements)
                    .service(arena::leaderboards::get_leaderboard)
                    .service(arena::avatar::set_avatar)
                    .service(arena::matchmaking::matchmaking_ws)
                    .service(arena::matchmaking::create_matchmaking_session)
                    .service(admin::import_character)
                    .service(
                        Files::new(
                            "/bundles.blades.bgs.services/",
                            static_data_clone.join("bundles.blades.bgs.services"),
                        )
                        .show_files_listing(),
                    )
            })
            .bind((host.as_str(), *port))
            .context("binding server")?
            .run();

            let srv_handle = srv_future.handle();
            select! {
                srv_result = srv_future => {
                    //TODO: politely close the enet thread
                    srv_result.context("server error")?;
                    bail!("actix-web server exited without error?");
                }
                enet_result = enet_future => {
                    srv_handle.stop(true).await;
                    enet_result.context("enet error")??;
                    bail!("enet exited without error?");
                }
            }
        }
    }
}
