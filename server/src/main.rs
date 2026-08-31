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
    web::{Data, JsonConfig},
};
use anyhow::{Context, Result};
use bb8::Pool;
use blades_lib::game_data::GameData;
use blades_lib::static_data::StaticData;
use blades_lib::features::level_up::LevelUpData;
use clap::{Parser, Subcommand};
use diesel_async::{AsyncPgConnection, pooled_connection::AsyncDieselConnectionManager};
use log::debug;

/// Connections in the Postgres pool. See the comment at the pool builder: the
/// database is configured with `max_connections = 20`, so this must stay well
/// under it.
const DB_POOL_MAX: u32 = 10;

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
mod guild_admin;
mod guild_policy;
mod inventory;
mod json_db;
pub mod models;
mod quest;
mod repair;
mod salvage;
pub mod schema;
mod shop;
mod shop_gen;
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
        /// Database connection string.
        ///
        /// Prefer the ENVIRONMENT. Passing it as an argument puts the database
        /// password in `/proc/<pid>/cmdline`, which is world-readable: any
        /// unprivileged local account can read it out of `ps aux`, and this box
        /// hosts more than this service. `hide_env_values` keeps it out of
        /// `--help` output too.
        #[arg(short, long, env = "ARENA_DATABASE_URL", hide_env_values = true)]
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

    /// Full ("max") durability per `(itemTemplateId, temperingLevel)` plus the
    /// gold price of a repair — `item_durability.json` + `repair_costs.json`, both
    /// generated from the APK by `script/extract_item_repair_data.py`.
    /// `GameData` (`parsed.json`) carries no durability, and the previous
    /// capture-derived table covered only 218 of 1113 templates at an average of
    /// 1.44 of the 11 temper levels, which made "Repair all" silently skip most
    /// items (tracker #30). Missing/invalid files load empty and repair falls back
    /// to [`blades_lib::features::repair::DEFAULT_DURABILITY`] rather than leaving
    /// gear damaged.
    pub repair_data: blades_lib::features::repair::RepairData,

    /// Faithful town game-data extracted from the APK bundles (server/data/static/):
    /// building upgrade cost/time/material tables, town job-pool definitions, and the
    /// appearance-change currency cost. Raw JSON parsed by the town/quest/character
    /// handlers; a missing/invalid file loads as `Null` and that feature degrades
    /// gracefully (no panic at startup).
    pub building_upgrades: serde_json::Value,
    /// The GLOBAL "pay gems to skip a running timer" curve, parsed once at startup
    /// out of `building_upgrades.json`'s `_meta.skipTimeCostTable` (the same
    /// `SkipTimeCostTable` asset retail used for BOTH town construction and
    /// crafting — see [`blades_lib::economy::skip_time`]).
    ///
    /// `None` when the static file predates the table — `deploy/static/` is a bind
    /// mount that merging does NOT ship, so this is the state between a merge and
    /// `deploy/arena.sh static`. Handlers then charge nothing rather than guessing a
    /// price: a few free speed-ups are recoverable, a wrong gem debit is not.
    pub skip_time_costs: Option<blades_lib::economy::skip_time::SkipTimeCostTable>,

    pub job_pools: serde_json::Value,

    pub appearance_change_cost: serde_json::Value,

    /// Authored, admin-editable per-level town-shop STOCK generation config
    /// (`shop_stock.json`). Drives [`shop_gen::generate_catalog`] so a vendor
    /// stocks level-appropriate items. A missing/invalid file loads as an empty
    /// config; the shop endpoint then falls back to the capture-derived templates
    /// (never empty). Pure data — a future admin route can hot-reload it.
    pub shop_stock: shop_gen::ShopStockConfig,

    /// What a town merchant pays for the player's items — APK `ItemTemplate.
    /// _sellValue` scaled by the temper multiplier, plus enchantment tier values
    /// (`item_sell_values.json` + `enchant_values.json`, both built by
    /// `script/extract_shop_economy_data.py`). Replaces a flat 50-gold-per-item
    /// placeholder that was ~75x below the retail median (tracker #30). Missing
    /// files load empty and the merchant offers 0, logged at startup.
    pub sell_prices: blades_lib::features::merchant::SellPrices,

    /// Data array containing item and currency arrays for levelup.
    pub level_up_data: blades_lib::features::level_up::LevelUpData,

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
            // Pool size is stated, not defaulted.
            //
            // Postgres on this box runs with `max_connections = 20` (measured,
            // not the 100 default). bb8's own default max_size is 10, so the
            // pool happened to fit — by coincidence, not design. Writing it
            // down means a bb8 upgrade cannot silently raise it past what the
            // database will accept, and it leaves headroom for the migration
            // one-shot and a psql session.
            //
            // Handlers acquire one connection and pass `&mut conn` down, so a
            // request never holds two; 10 is concurrency, not a per-request
            // cost.
            let db_pool = Pool::builder()
                .max_size(DB_POOL_MAX)
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

            // Repair needs each item's full durability and gold price, neither of
            // which `parsed.json` carries. Both tables are generated from the APK
            // by `script/extract_item_repair_data.py`. A missing or invalid file
            // loads as an empty table rather than panicking at startup; repair then
            // falls back to the game's own DEFAULT_DURABILITY, so gear is still
            // restored to full instead of being silently left damaged.
            let repair_data = {
                let load = |name: &str| -> serde_json::Value {
                    let p = static_data.join(name);
                    match File::open(&p) {
                        Ok(f) => serde_json::from_reader(std::io::BufReader::new(f))
                            .unwrap_or_else(|e| {
                                log::warn!("[repair] invalid {p:?}: {e}; falling back to defaults");
                                serde_json::Value::Null
                            }),
                        Err(e) => {
                            log::warn!("[repair] no {p:?}: {e}; falling back to defaults");
                            serde_json::Value::Null
                        }
                    }
                };
                let data = blades_lib::features::repair::RepairData::from_json(
                    &load("item_durability.json"),
                    &load("repair_costs.json"),
                );
                log::info!(
                    "[repair] durability table: {} templates; repair-cost table: {} templates",
                    data.template_count(),
                    data.cost_template_count()
                );
                data
            };

            // Faithful town game-data extracted from the APK (building upgrade costs,
            // job pools, appearance-change cost). Missing/invalid → Null; the consuming
            // handler degrades gracefully rather than panicking at startup.
            let load_static_json = |name: &str| -> serde_json::Value {
                let p = static_data.join(name);
                match File::open(&p) {
                    Ok(f) => serde_json::from_reader(std::io::BufReader::new(f))
                        .unwrap_or_else(|e| {
                            log::warn!("[static] invalid {p:?}: {e}; feature degraded to Null");
                            serde_json::Value::Null
                        }),
                    Err(_) => {
                        log::warn!("[static] no {p:?}; feature degraded to Null");
                        serde_json::Value::Null
                    }
                }
            };
            let building_upgrades = load_static_json("building_upgrades.json");
            // One global skip-time curve for town construction AND crafting, parsed
            // once here so the handlers don't re-walk the JSON per request. Logged
            // loudly when absent, because "absent" means every speed-up is free —
            // `deploy/static/` is a bind mount and a merge alone does not ship it.
            let skip_time_costs =
                blades_lib::economy::skip_time::SkipTimeCostTable::from_static(&building_upgrades);
            match &skip_time_costs {
                Some(t) => log::info!(
                    "[static] skip-time cost table loaded ({} bands)",
                    t.rate_list.len()
                ),
                None => log::warn!(
                    "[static] building_upgrades.json has no usable _meta.skipTimeCostTable; \
                     speed-ups (speedUp:true) will be FREE until `deploy/arena.sh static` runs"
                ),
            }
            let job_pools = load_static_json("job_pools.json");
            let appearance_change_cost = load_static_json("appearance_change_cost.json");

            // Authored per-level shop-stock generation config. Parsed straight into
            // the typed `ShopStockConfig` (only the `generation` block is read);
            // missing/invalid → empty config and the shop endpoint falls back to the
            // capture-derived templates rather than panicking at startup.
            let shop_stock: shop_gen::ShopStockConfig = {
                let p = static_data.join("shop_stock.json");
                match File::open(&p) {
                    Ok(f) => serde_json::from_reader(std::io::BufReader::new(f))
                        .unwrap_or_else(|e| {
                            log::warn!(
                                "[shop] invalid {p:?}: {e}; shop stock falls back to templates"
                            );
                            Default::default()
                        }),
                    Err(_) => {
                        log::warn!("[shop] no {p:?}; shop stock falls back to templates");
                        Default::default()
                    }
                }
            };

            // What a town merchant pays for the player's items. Missing/invalid →
            // empty tables and the merchant offers 0 (logged), rather than a panic.
            let sell_prices = {
                let data = blades_lib::features::merchant::SellPrices::from_json(
                    &load_static_json("item_sell_values.json"),
                    &load_static_json("enchant_values.json"),
                );
                if data.is_empty() {
                    log::warn!(
                        "[shop] no sell-price table; town merchants will offer 0 gold for \
                         the player's items"
                    );
                } else {
                    log::info!(
                        "[shop] sell prices: {} templates, {} enchantment properties",
                        data.template_count(),
                        data.enchant_property_count()
                    );
                }
                data
            };

            let level_up_data = LevelUpData::from_json(&load_static_json("level_rewards.json"));

            // Capture-derived static definitions (gifts, announcements, …). Missing
            // files degrade gracefully (empty → endpoint returns an empty list).
            let static_data_defs = static_loader::load(&static_data);

            let arena = arena::matchmaker::ArenaGlobal::start(
                arena::config::ArenaConfig::from_env(),
                db_pool.clone(),
            );

            // Phase 5.4 — start the match-end economy writer. The combat engine is
            // synchronous and cannot touch the async pool from inside the ENet tick,
            // so it pushes finished matches onto a queue that this task drains.
            // Without it the victory card's gold / XP / trophies are wire-only and
            // vanish the moment the client re-syncs from REST.
            arena::arena_economy::install(db_pool.clone());

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
                repair_data,
                building_upgrades,
                skip_time_costs,
                job_pools,
                appearance_change_cost,
                shop_stock,
                sell_prices,
                level_up_data,
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
                    // Per-request timing. There was NO request logging at all,
                    // which is why "the first shop took very long" could only be
                    // guessed at: nothing recorded how long anything took.
                    //
                    // `%D` is the handler's own time in milliseconds, so a slow
                    // response can be separated from a slow client or a slow
                    // link. Quiet by default — set `RUST_LOG=actix_web=info` to
                    // turn it on — because this logs one line per request and
                    // the game polls hard (one player's session was 624
                    // manifest fetches).
                    .wrap(actix_web::middleware::Logger::new(
                        "%s %r %Dms",
                    ))
                    .app_data(Data::new(server_global.clone()))
                    // A character transfer POSTs the whole CompleteCharacter, and
                    // actix's default JSON limit is 2 MiB — which is smaller than a
                    // real progressed character. A level-28 import was rejected with
                    // `JSON payload (3094666 bytes) is larger than allowed
                    // (limit: 2097152 bytes)`, leaving that player unable to be
                    // restored while 54 others succeeded.
                    //
                    // The stored document is only ~390 KiB at its largest across all
                    // 101 captured alts; the transfer payload inflates it roughly 8x
                    // by expanding inventory and quest state. 16 MiB is ~5x the
                    // largest payload actually observed, so it clears real characters
                    // with room to spare while still bounding an unbounded body.
                    .app_data(JsonConfig::default().limit(16 * 1024 * 1024))
                    .wrap_fn(|mut req, srv| {
                        let start_timestamp = SystemTime::now();
                        let is_from_blades_api =
                            req.uri().path().starts_with("/api/");
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
                    .service(authentification::refresh)
                    .service(character::list_characters)
                    .service(character::create_characters)
                    .service(character::get_character)
                    .service(wallet::get_wallet)
                    .service(inventory::get_inventory)
                    .service(analytics_events::list_events)
                    .service(dungeon::get_dungeons)
                    .service(dungeon::enter_quest_dungeon)
                    .service(dungeon::exit_quest_dungeon)
                    .service(dungeon_update::dungeon_update)
                    // Abyss: specific sub-paths (start/update/end) BEFORE the bare /current.
                    .service(abyss::start_abyss)
                    .service(abyss::update_abyss)
                    .service(abyss::end_abyss)
                    .service(abyss::get_abyss)
                    .service(town::get_town)
                    // Town building lifecycle. Register the deeper `/buildings/{id}/…`
                    // paths BEFORE the bare `/buildings` collection POST so the latter
                    // (place) doesn't shadow the per-building actions.
                    .service(town::upgrade_building)
                    .service(town::complete_building)
                    .service(town::destroy_building)
                    .service(town::set_building_style)
                    .service(town::place_building)
                    .service(town::remove_town_props)
                    .service(town::place_town_props)
                    .service(town::set_town_name)
                    // Crafting: finish (specific path) BEFORE create (bare /crafts).
                    .service(craft::finish_craft)
                    .service(craft::create_craft)
                    .service(craft::get_crafts)
                    .service(repair::repair_items)
                    .service(salvage::salvage_items)
                    // Vendor shops: specific verbs before the bare `/shops/{id}` open.
                    .service(shop::buy_from_shop)
                    .service(shop::sell_to_shop)
                    .service(shop::refresh_loot)
                    .service(shop::open_shop)
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
                    // Quest completion and objective progress must come AFTER accept
                    // (longer path → won't shadow the bare /quests POST).
                    .service(quest::complete_quest)
                    .service(quest::update_quest_objectives)
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
                    // Guild: literal paths (current/search/leaderboard/exchanges/…)
                    // MUST precede the generic `/guilds/{guild_id}` so they aren't
                    // captured by it. Exchange sub-paths (donate/redeem) must come
                    // before the bare `exchanges` POST.
                    .service(guild::get_current_guild)
                    .service(guild::update_guild_post)
                    .service(guild::update_guild_put)
                    .service(guild::search_guilds)
                    .service(guild::guild_leaderboard)
                    .service(guild::get_messages)
                    .service(guild::post_message)
                    .service(guild::list_applications)
                    .service(guild::approve_application)
                    .service(guild::deny_application)
                    .service(guild::leave_guild)
                    .service(guild::kick_member)
                    .service(guild::ban_member)
                    .service(guild::list_exchanges)
                    .service(guild::donate_exchange)
                    .service(guild::redeem_exchange)
                    .service(guild::create_exchange)
                    .service(guild::create_guild)
                    // `{id}/join` and `{id}/apply` before the bare `{id}` GET.
                    .service(guild::join_guild)
                    .service(guild::apply_to_guild)
                    .service(guild::get_guild)
                    .service(announcements::get_announcements)
                    .service(arena::leaderboards::get_leaderboard)
                    .service(arena::presence::arena_presence)
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
                    .service(admin::arena_season_rollover)
                    // Guild support console (dev-token gated; the web /guilds
                    // page is its only intended caller).
                    .service(guild_admin::list_guilds)
                    .service(guild_admin::set_grandmaster)
                    // Registered AFTER the /grandmaster route: `{guild_id}` is a
                    // greedy single segment, so the more specific path must be
                    // offered to the router first or a handover POST would be
                    // swallowed by the detail GET's scope.
                    .service(guild_admin::get_guild_detail)
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

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::Parser;

    /// ONE lock for the whole module. Rust runs tests in parallel threads that
    /// share a single process environment, so a per-test lock would serialise
    /// nothing — which is exactly the bug the first version of these tests had.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The database password must reach the server through the ENVIRONMENT.
    ///
    /// Passing it as an argument puts it in `/proc/<pid>/cmdline`, which any
    /// unprivileged local account can read out of `ps aux` — and this box hosts
    /// more than this service, so it was a real exposure. This test fails if
    /// someone removes the `env` attribute and quietly sends us back to argv.
    ///
    /// Serialised and restored around the env mutation: Rust runs tests in
    /// parallel threads sharing one environment.
    #[test]
    fn the_connection_string_can_come_from_the_environment() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("ARENA_DATABASE_URL").ok();

        unsafe { std::env::set_var("ARENA_DATABASE_URL", "postgres://u:p@h:5432/db") };
        let parsed = Cli::try_parse_from([
            "server", "run", "--host", "0.0.0.0", "--port", "8080",
            "--static-data", "/data/static",
        ]);
        match prev {
            Some(v) => unsafe { std::env::set_var("ARENA_DATABASE_URL", v) },
            None => unsafe { std::env::remove_var("ARENA_DATABASE_URL") },
        }

        let cli = parsed.expect("the env var must satisfy --connection-string");
        let Commands::Run { connection_string, .. } = cli.command;
        assert_eq!(connection_string, "postgres://u:p@h:5432/db");
    }

    /// With neither the flag nor the variable it must still refuse to start.
    /// A fallback that quietly defaulted to something would be worse than the
    /// exposure it replaced.
    #[test]
    fn it_still_refuses_to_start_with_no_connection_string_at_all() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("ARENA_DATABASE_URL").ok();
        unsafe { std::env::remove_var("ARENA_DATABASE_URL") };

        let parsed = Cli::try_parse_from([
            "server", "run", "--host", "0.0.0.0", "--port", "8080",
            "--static-data", "/data/static",
        ]);
        if let Some(v) = prev {
            unsafe { std::env::set_var("ARENA_DATABASE_URL", v) };
        }
        assert!(parsed.is_err(), "must not start without a connection string");
    }
}
