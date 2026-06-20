use std::{
    fs::File,
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
use anyhow::{Context, Result};
use bb8::Pool;
use blades_lib::game_data::GameData;
use blades_lib::static_data::StaticData;
use clap::{Parser, Subcommand};
use diesel_async::{AsyncPgConnection, pooled_connection::AsyncDieselConnectionManager};
use log::debug;

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
mod character_ops;
mod chests;
mod craft;
mod daily_reward;
mod dungeon;
mod dungeon_update;
mod error;
mod gameevent;
mod global_gift;
mod global_shop;
mod guild;
mod inventory;
mod json_db;
pub mod models;
mod quest;
mod repair;
mod salvage;
pub mod schema;
mod session;
mod static_loader;
mod status;
mod town;
mod util;
mod wallet;

pub use error::BladeApiError;
use uuid::Uuid;

use crate::session::{SessionLookedUpMaybe, SessionStore};

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
    },
}

pub type DbPool = Pool<AsyncDieselConnectionManager<AsyncPgConnection>>;

pub struct ServerGlobal {
    pub db_pool: DbPool,
    pub session_store: SessionStore,
    pub static_data_path: PathBuf,
    pub game_data: GameData,
    /// Capture-derived static definitions (gifts, announcements, …) loaded at
    /// startup from JSON files in the `--static-data` directory. Empty parts
    /// degrade gracefully (see [`static_loader`]).
    pub static_data: StaticData,
    /// Full ("max") durability per `(itemTemplateId, temperingLevel)`, derived
    /// from the captures (`item_durability.json`) since `GameData` carries no
    /// durability. Used by the repair endpoint to restore an item to full.
    /// Keyed by lowercase UUID string -> tempering-level string -> max durability.
    /// Empty if the file is missing/invalid (repair then leaves durability as-is).
    pub item_max_durability: std::collections::HashMap<String, std::collections::HashMap<String, f64>>,
    pub arena: Arc<arena::matchmaker::ArenaGlobal>,
    /// Static dev token for the `/api/dev/v1/import-character` endpoint, read
    /// from `ARENA_IMPORT_TOKEN` at startup. `None` (unset) disables the
    /// endpoint entirely. Never a game session — this is for our own tooling.
    pub arena_import_token: Option<String>,
    /// **DEBUG.** Token gating the experimental arena packet-injection routes
    /// (`/arena/debug/{peers,inject}`), read from `ARENA_DEBUG_TOKEN`. When unset,
    /// those routes fall back to `arena_import_token`; with neither set they 503
    /// (disabled). For our own debugging only — never a game session.
    pub arena_debug_token: Option<String>,
    /// Dev override: when set (env `ARENA_DEV_LOGIN_USER_ID` = a `users.id` UUID),
    /// EVERY anonymous login resolves to this user, so a freshly-installed client
    /// lands on a Transfer'd character instead of a new empty account (there is no
    /// Bethesda/Google identity to map to). Unset in normal operation.
    pub dev_login_user_id: Option<uuid::Uuid>,
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

            // Repair needs each item's full durability, which `parsed.json` does
            // not carry — load the captures-derived lookup. Tolerate a missing or
            // invalid file (empty map → repair leaves durability unchanged rather
            // than panicking the server at startup).
            let item_max_durability: std::collections::HashMap<
                String,
                std::collections::HashMap<String, f64>,
            > = {
                let p = static_data.join("item_durability.json");
                match File::open(&p) {
                    Ok(f) => serde_json::from_reader(std::io::BufReader::new(f))
                        .unwrap_or_else(|e| {
                            log::warn!(
                                "[durability] invalid {p:?}: {e}; repair will leave durability unchanged"
                            );
                            Default::default()
                        }),
                    Err(e) => {
                        log::warn!(
                            "[durability] no {p:?}: {e}; repair will leave durability unchanged"
                        );
                        Default::default()
                    }
                }
            };

            // Capture-derived static definitions (gifts, announcements, …). Missing
            // files degrade gracefully (empty → endpoint returns an empty list).
            let static_data_defs = static_loader::load(&static_data);

            let arena = arena::matchmaker::ArenaGlobal::start(
                arena::config::ArenaConfig::from_env(),
                db_pool.clone(),
            );

            let arena_import_token = std::env::var("ARENA_IMPORT_TOKEN").ok();
            // DEBUG: dedicated token for the arena packet-injection routes;
            // falls back to ARENA_IMPORT_TOKEN in the handler when unset.
            let arena_debug_token = std::env::var("ARENA_DEBUG_TOKEN").ok();
            // Dev override: pin every anon login to one user (a Transfer'd character).
            let dev_login_user_id = std::env::var("ARENA_DEV_LOGIN_USER_ID")
                .ok()
                .and_then(|s| uuid::Uuid::parse_str(s.trim()).ok());

            let server_global = Arc::new(ServerGlobal {
                db_pool,
                session_store: SessionStore::new(Duration::from_hours(24)),
                static_data_path: static_data.clone(),
                game_data,
                static_data: static_data_defs,
                item_max_durability,
                arena,
                arena_import_token,
                arena_debug_token,
                dev_login_user_id,
            });

            // Live arena ENet host (real-client path) — needs the shared Arc.
            let enet_globals = server_global.clone();
            actix_web::rt::spawn(async move {
                if let Err(e) = arena::enet_host::run_enet_host(enet_globals).await {
                    log::error!("arena-enet host exited: {e}");
                }
            });

            let static_data_clone = static_data.clone();

            HttpServer::new(move || {
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
                    .service(repair::repair_items)
                    .service(salvage::salvage_items)
                    .service(challenge::get_challenges)
                    .service(challenge::update_challenge)
                    .service(challenge::complete_challenge)
                    .service(challenge::abandon_challenge)
                    .service(character_ops::levelup)
                    .service(character_ops::learn_abilities)
                    .service(character_ops::respec)
                    .service(character_ops::upgrade_inventory)
                    .service(character_ops::destroy_items)
                    .service(character_ops::save_loadout_profile)
                    .service(character_ops::update_loadout)
                    .service(gameevent::get_game_events)
                    .service(quest::get_quests)
                    .service(quest::accept_quest)
                    .service(global_shop::get_override)
                    .service(global_shop::get_global_shop_for_character)
                    .service(global_shop::get_iap)
                    .service(global_shop::purchase_global_shop)
                    .service(global_gift::get_global_gifts)
                    .service(global_gift::get_global_gift)
                    .service(global_gift::claim_global_gift)
                    .service(character_data::update_data)
                    .service(daily_reward::get_daily_reward)
                    .service(daily_reward::collect_daily_reward)
                    .service(chests::collect_chest)
                    // Guild: literal paths (current/search/leaderboard/…) MUST precede
                    // the generic `/guilds/{guild_id}` so they aren't captured by it.
                    .service(guild::get_current_guild)
                    .service(guild::search_guilds)
                    .service(guild::guild_leaderboard)
                    .service(guild::get_messages)
                    .service(guild::post_message)
                    .service(guild::leave_guild)
                    .service(guild::kick_member)
                    .service(guild::create_guild)
                    .service(guild::join_guild)
                    .service(guild::get_guild)
                    .service(announcements::get_announcements)
                    .service(arena::leaderboards::get_leaderboard)
                    .service(arena::avatar::set_avatar)
                    .service(arena::matchmaking::matchmaking_ws)
                    .service(arena::matchmaker::create_match)
                    .service(arena::matchmaker::cancel_match)
                    // DEBUG/experimental packet-injection harness (token-gated).
                    .service(arena::debug_inject::debug_peers)
                    .service(arena::debug_inject::debug_inject)
                    .service(admin::import_character)
                    .service(admin::recent_matches)
                    .service(admin::bind_device)
                    .service(admin::recent_devices)
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
            .run()
            .await
            .context("running the server")?;
        }
    }

    Ok(())
}
