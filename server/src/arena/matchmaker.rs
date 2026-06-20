//! Matchmaker actor + the matchmaking REST surface.
//!
//! Flow (confirmed from captured prod traffic):
//!   1. client POSTs `matches/create` → we mint a ticketId, enqueue it, and
//!      return `{match:{ticketId,status:"QUEUED",port:0}}`.
//!   2. the matchmaker pushes three frames over the client's RMS WebSocket:
//!      `Searching` → `PotentialMatch` → `Succeeded{address,port,...}`.
//!   3. (cancellation) client POSTs `matches/{ticketId}/cancel` → `null`.
//!
//! v1 is solo + bot: a single ticket forms a match immediately and `Succeeded`
//! points at our configured arena UDP endpoint. Real pairing + the live UDP
//! match instance land in milestone (c)/(d).

use std::sync::Arc;
use std::time::Duration;

use actix_web::{
    HttpResponse, post,
    http::StatusCode,
    web::{self, Json},
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::RunQueryDsl;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use uuid::Uuid;

use crate::{
    BladeApiError, DbPool, ServerGlobal,
    arena::{
        MatchmakingMessage,
        config::ArenaConfig,
        key_submit::{KeySubmitConfig, KeySubmitter},
        match_registry::MatchRegistry,
    },
    models::CharacterDbEntryCharacterWalletInventory,
    schema::characters,
    session::SessionLookedUpMaybe,
};

/// A queued matchmaking ticket handed to the matchmaker actor. Carries a clone
/// of the requesting session's RMS sender so the matchmaker can push frames
/// back to exactly that client.
pub struct TicketRequest {
    pub ticket_id: Uuid,
    pub user_id: Uuid,
    pub rms: UnboundedSender<MatchmakingMessage>,
}

/// Status of a recorded matchmaking ticket, for the web /arena activity feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RecentStatus {
    /// Queued, waiting for an opponent (or the solo-fallback timer).
    Searching,
    /// Resolved into a match (solo/bot or a PvP pair).
    Matched,
}

/// A JSON view of a recent ticket (what the dev `recent-matches` endpoint — and
/// thus the web /arena page — sees). The requesting user is shown only as an
/// opaque short tag (first 8 hex of the arena user id), never full identity.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentTicketView {
    pub ticket_id: Uuid,
    pub user_tag: String,
    pub status: RecentStatus,
    pub paired: bool,
    pub game_session_id: Option<Uuid>,
    pub age_seconds: u64,
    /// True when this ticket's user == the `userId` query filter (i.e. "you").
    pub mine: bool,
}

/// A row of the durable `arena_matches` table (migration 2026-06-16_add_arena_matches),
/// read back for the `recent-matches` endpoint. `age_seconds` is computed in SQL
/// (`now() - recorded_at`) so it survives restarts — unlike the old in-memory Instant.
#[derive(diesel::QueryableByName)]
struct ArenaMatchRow {
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    ticket_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Uuid)]
    user_id: Uuid,
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    paired: bool,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Uuid>)]
    game_session_id: Option<Uuid>,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    age_seconds: i64,
}

/// Record a newly-queued ticket as `searching` in `arena_matches`. Best-effort:
/// matchmaking must not block on (or fail from) the DB, so pool/SQL errors are
/// logged and swallowed. No-op when `db` is None (the unit test has no DB).
async fn record_match_queued(db: &Option<DbPool>, ticket_id: Uuid, user_id: Uuid) {
    let Some(db) = db else { return };
    let Ok(mut conn) = db.get().await else {
        warn!("arena_matches: db pool unavailable (queued {ticket_id})");
        return;
    };
    if let Err(e) = diesel::sql_query(
        "INSERT INTO arena_matches (ticket_id, user_id, status, recorded_at) \
         VALUES ($1, $2, 'searching', now()) ON CONFLICT (ticket_id) DO NOTHING",
    )
    .bind::<diesel::sql_types::Uuid, _>(ticket_id)
    .bind::<diesel::sql_types::Uuid, _>(user_id)
    .execute(&mut conn)
    .await
    {
        warn!("arena_matches: insert failed ({ticket_id}): {e}");
    }
}

/// Mark a ticket `matched` (solo or paired) in `arena_matches`. Best-effort.
async fn record_match_resolved(
    db: &Option<DbPool>,
    ticket_id: Uuid,
    game_session_id: Uuid,
    paired: bool,
) {
    let Some(db) = db else { return };
    let Ok(mut conn) = db.get().await else { return };
    let _ = diesel::sql_query(
        "UPDATE arena_matches SET status='matched', game_session_id=$2, paired=$3, \
         resolved_at=now() WHERE ticket_id=$1",
    )
    .bind::<diesel::sql_types::Uuid, _>(ticket_id)
    .bind::<diesel::sql_types::Uuid, _>(game_session_id)
    .bind::<diesel::sql_types::Bool, _>(paired)
    .execute(&mut conn)
    .await;
}

/// Load a player's combat loadout (equipped abilities + weapon damage enchants)
/// from their character row. **NOT called from the matchmaker path** — awaiting it
/// inline on the single matchmaker actor hung all matchmaking (see `resolve`).
/// Re-enable only OFF the actor: a spawned task, a bounded `tokio::time::timeout`,
/// and/or a per-user cache, so a slow `characters` query can't stall matches.
#[allow(dead_code)]
async fn load_loadout(db: &Option<DbPool>, user_id: Uuid) -> crate::arena::combat::Loadout {
    use crate::arena::combat::loadout;
    let Some(db) = db else {
        return loadout::starter();
    };
    let Ok(mut conn) = db.get().await else {
        return loadout::starter();
    };
    let row = characters::table
        .filter(characters::user_id.eq(user_id))
        .select(CharacterDbEntryCharacterWalletInventory::as_select())
        .load(&mut conn)
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next());
    match row {
        Some(r) => {
            let mut lo = loadout::from_character(&r.character.0, &r.inventory.0);
            lo.character_uuid = r.id.to_string(); // the character UUID for the op50 spawn
            // op54 round-start PROFILE JSON, serialized faithfully from the stored
            // character (the structs ARE the game wire format — camelCase + verbatim
            // Value sub-objects): p4 = {"equippedItems":{…}}, p5 = data + id + character.
            // MUST include `data` (customization) — retail's profile carries it
            // (data.customization.CharacterUID = the avatar visual); without it the
            // opponent has no appearance and the client's resource-load hangs at
            // "connecting" (the 2026-06-17 gate, found via the on-device matchstate probe).
            // TODO(arena-profile): `equippedItems` shape still diverges from retail
            // (missing per-item `grade` / `arcaneTier`). Fixing it needs data-model
            // changes (those fields aren't stored on the item), so it's a separate
            // follow-up — left as-is for now. The op54 hang is driven by the CHARACTER
            // JSON schema (below), which this fix makes retail-identical.
            lo.profile_equipped_json =
                serde_json::json!({ "equippedItems": &r.inventory.0.loadout.equipped_items }).to_string();
            // op54 round-start PROFILE character JSON, trimmed to retail's schema.
            lo.profile_character_json =
                build_profile_character_json(&r.data.0, r.id, &r.character.0);
            lo
        }
        None => loadout::starter(),
    }
}

/// Build the op54 round-start PROFILE character JSON, **trimmed to retail's
/// schema**. Retail's opponent profile is rejected by the client's deserializer
/// when it carries keys retail never sends (capture-proven by the field-diff of
/// session 506: the client then never loads the opponent's resources and the
/// match hangs at "Connecting").
///
/// We serialize the same `CompleteCharacterWithIdAndData` the rest of the server
/// uses (the structs ARE the camelCase wire format — see `blades_lib`), then
/// post-process the JSON `Value` so the profile is schema-identical to retail —
/// WITHOUT touching the global structs (they back GET /character, transfers,
/// initial sync, etc.; this transform is profile-specific):
///   - drop the top-level `challengeSeason` key (retail's profile has none);
///   - replace `data` with an object containing ONLY `customization` — drop
///     `dialog` and `new-flags` entirely (retail's `data` is customization-only;
///     `customization.CharacterUID` = the opponent's avatar appearance and is
///     preserved verbatim).
///
/// On any (unexpected) serialize/shape error this returns whatever serialized,
/// matching the previous `unwrap_or_default()` behaviour (never panics the actor).
fn build_profile_character_json(
    data: &blades_lib::user_data::CompleteCharacterData,
    id: Uuid,
    character: &blades_lib::user_data::CompleteCharacter,
) -> String {
    let serialized = match serde_json::to_string(
        &blades_lib::user_data::CompleteCharacterWithIdAndData {
            data: data.clone(),
            id,
            character: character.clone(),
        },
    ) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&serialized) else {
        return serialized;
    };
    if let Some(obj) = value.as_object_mut() {
        // retail's profile has no `challengeSeason`, `completedQuests`, or
        // `globalShopOffers` — capture-proven: across ALL 830 op54 PROFILE frames in
        // the capture DB (s506 etc.) none of these three top-level keys ever appears.
        // The client's profile deserializer rejects an opponent profile that carries
        // keys retail never sends, so `OnUserMessage` never fires, the opponent's
        // loadout/appearance never loads, and the match hangs at "Connecting". Our
        // profile was 31047 B (34700 B on the wire, 26 ENet fragments) vs retail's
        // 17008 B (20776 B, 16 fragments); `completedQuests` (~4.9 KB) was the bulk of
        // the divergence. Dropping these matches retail's exact profile schema.
        // [diffed live 2026-06-19: WolfWalker s2c op54 vs retail s506 op54 char "Blank".]
        obj.remove("challengeSeason");
        obj.remove("completedQuests");
        obj.remove("globalShopOffers");
        // retail's `data` is `customization`-only — rebuild it from scratch so
        // `dialog` / `new-flags` are dropped, not blanked.
        let customization = obj
            .get("data")
            .and_then(|d| d.get("customization"))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        obj.insert(
            "data".to_string(),
            serde_json::json!({ "customization": customization }),
        );
    }
    serde_json::to_string(&value).unwrap_or(serialized)
}

/// True iff the ghost would be a **self-match** against the human — i.e. they
/// resolve to the SAME (non-empty) character UUID. The client links each Avatar
/// net-object to its Player by this UUID (the op50 spawn `p4`), so a ghost whose
/// `CharacterUID` equals the local player's cannot be built as a *distinct*
/// opponent actor: `PvpEncounter.SpawnOpponent`/`OnOpponentLoaded` never fires and
/// the match hangs at "Connecting" even though both players' resources load.
/// (Empty UUIDs — a starter loadout — never count as a self-match.)
fn is_self_match(human_char_uuid: &str, ghost_char_uuid: &str) -> bool {
    !ghost_char_uuid.is_empty() && ghost_char_uuid == human_char_uuid
}

/// Derive one `playerSessionId` per player for a match, sharing the `gameSessionId`.
///
/// playerSessionId shape (retail GameLift, capture-confirmed s506
/// `psess-0a7c4b72-0a1c-b2c9-6599-05c28c5ed98e`): the first three UUID groups are
/// DERIVED FROM the shared `gameSessionId`, so paired players' psess share a common
/// `psess-<gsid g1>-<gsid g2>-<gsid g3>-…` prefix, and only the last two groups (the
/// per-player suffix) differ. We previously minted a fully-independent `psess-<new
/// uuid>` per player, so paired players shared no prefix — a divergence from retail
/// that any server-side gsid↔psess correlation (e.g. session lookup) would miss.
/// [docs/arena-journey-log.md §7]
fn derive_player_session_ids(game_session_id: Uuid, count: usize) -> Vec<String> {
    let gsid = game_session_id.to_string(); // canonical 8-4-4-4-12 lowercase hyphenated
    let gsid_prefix: String = gsid.splitn(4, '-').take(3).collect::<Vec<_>>().join("-");
    (0..count)
        .map(|_| {
            // Per-player suffix = the last two groups of a fresh UUID (4 + 12 hex).
            let suffix: String = {
                let u = Uuid::new_v4().to_string();
                u.splitn(4, '-').skip(3).collect::<Vec<_>>().join("-")
            };
            format!("psess-{gsid_prefix}-{suffix}")
        })
        .collect()
}

/// Validate a REAL-PAIRED (human-vs-human) match's per-fighter binding UUIDs before
/// allocation — the appearance-swap guard (`docs/arena-appearance-bug-spec.md`).
///
/// The client binds each opponent's APPEARANCE entirely by the avatar net-object's
/// `propId4` character-UUID (`PvpClientManager.GetPvpPlayer(<avatar.propId4>)` →
/// that player's op54 customization), but binds NAMES off the Player object directly
/// — so a broken avatar→player UUID binding corrupts appearance while leaving names
/// intact (exactly the reported symptom). The binding collapses (both avatars resolve
/// to the LOCAL `PvpPlayer`) whenever the two fighters carry the SAME — or an EMPTY —
/// `character_uuid`:
///   - **two equal non-empty UUIDs** (both peers resolved to the same `characters`
///     row) → `GetPvpPlayer` returns the first-registered (local) player for BOTH
///     avatars → each client renders the opponent with its OWN appearance;
///   - **an empty UUID** (a `starter()` fallback on a slow/missing `load_loadout`)
///     → `spawn_avatar` emits `propId4 = ""`, which can't bind a distinct opponent
///     AND drops the opponent profile (`broadcast_profiles` skips empty profiles).
///
/// Mirrors the existing ghost-path [`is_self_match`] guard, but for the human pair.
/// `Ok(())` when every fighter has a distinct, non-empty `character_uuid`; otherwise
/// `Err(reason)` so the caller can refuse to ship a known-collapsed match. Bots are
/// excluded (a solo-vs-bot match is the ghost path's concern, not this one).
fn check_paired_uuids_distinct(loadouts: &[crate::arena::combat::Loadout]) -> Result<(), String> {
    for (i, lo) in loadouts.iter().enumerate() {
        if lo.character_uuid.is_empty() {
            return Err(format!(
                "fighter {i} (\"{}\") has an EMPTY character_uuid — its avatar's propId4 would \
                 be \"\", which can't bind a distinct opponent (appearance collapses to the local \
                 char) and drops its op54 profile. A paired fighter must carry its own non-empty \
                 character UUID before round-start.",
                lo.display_name,
            ));
        }
        for (j, other) in loadouts.iter().enumerate().skip(i + 1) {
            if lo.character_uuid == other.character_uuid {
                return Err(format!(
                    "fighters {i} (\"{}\") and {j} (\"{}\") share character_uuid {} — both avatars' \
                     propId4 would be identical, so GetPvpPlayer collapses both onto the local \
                     PvpPlayer and each client renders the opponent with its OWN appearance \
                     (names stay correct). The two peers resolved to the SAME characters row.",
                    lo.display_name, other.display_name, lo.character_uuid,
                ));
            }
        }
    }
    Ok(())
}

/// Newest-first view of `arena_matches`, capped at `limit`, marking `mine`
/// against `filter`. Backs the dev `recent-matches` endpoint; durable across
/// restarts (#NB-3). Returns empty on a DB error (the endpoint stays up).
pub async fn query_recent_matches(
    db: &DbPool,
    limit: i64,
    filter: Option<Uuid>,
) -> Vec<RecentTicketView> {
    let Ok(mut conn) = db.get().await else { return Vec::new() };
    let rows: Vec<ArenaMatchRow> = diesel::sql_query(
        "SELECT ticket_id, user_id, status, paired, game_session_id, \
         CAST(EXTRACT(epoch FROM (now() - recorded_at)) AS BIGINT) AS age_seconds \
         FROM arena_matches ORDER BY recorded_at DESC LIMIT $1",
    )
    .bind::<diesel::sql_types::BigInt, _>(limit)
    .get_results(&mut conn)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(|r| {
            let id_str = r.user_id.to_string();
            RecentTicketView {
                ticket_id: r.ticket_id,
                user_tag: id_str[..8].to_string(),
                status: if r.status == "matched" {
                    RecentStatus::Matched
                } else {
                    RecentStatus::Searching
                },
                paired: r.paired,
                game_session_id: r.game_session_id,
                age_seconds: r.age_seconds.max(0) as u64,
                mine: filter.map(|f| f == r.user_id).unwrap_or(false),
            }
        })
        .collect()
}

/// Shared arena state (hung off `ServerGlobal`). Cloning the `UnboundedSender`
/// is the only thing request handlers touch — the queue itself lives inside the
/// single-owner matchmaker task.
pub struct ArenaGlobal {
    pub config: ArenaConfig,
    pub matchmaker_tx: UnboundedSender<TicketRequest>,
    pub registry: Arc<MatchRegistry>,
}

impl ArenaGlobal {
    /// Build the arena subsystem and spawn the matchmaker actor on the current
    /// arbiter. Returns the shared handle to store in `ServerGlobal`.
    pub fn start(config: ArenaConfig, db_pool: DbPool) -> Arc<Self> {
        // Build the per-match key submitter (captures the current tokio runtime
        // handle — `start` runs under the actix/tokio runtime). `None` when
        // submission is disabled / unconfigured, in which case admit is a no-op.
        let key_submitter = KeySubmitter::from_config(KeySubmitConfig::from_env()).map(Arc::new);
        let registry = MatchRegistry::new_with_submitter(config.max_concurrent_matches, key_submitter);

        let (tx, rx) = unbounded_channel::<TicketRequest>();
        let mm_cfg = config.clone();
        let mm_reg = registry.clone();
        actix_web::rt::spawn(async move {
            matchmaker_loop(rx, mm_cfg, mm_reg, Some(db_pool)).await;
        });
        // The live ENet arena host (tokio-enet) is spawned from main() once
        // ServerGlobal exists (it needs the shared Arc). `udp.rs`'s raw-socket
        // UdpServer is the dev/test reference for the crypto + FSM unit tests.

        Arc::new(ArenaGlobal {
            config,
            matchmaker_tx: tx,
            registry,
        })
    }
}

/// The matchmaker actor. Single owner of the ticket queue — no locks. A lone ticket
/// waits `config.solo_fallback_secs` for a human opponent to PAIR with; if none
/// arrives it falls back to a solo match against a server-driven bot, so a single
/// tester always gets a fight instead of being stuck "Searching".
async fn matchmaker_loop(
    mut rx: UnboundedReceiver<TicketRequest>,
    config: ArenaConfig,
    registry: Arc<MatchRegistry>,
    db: Option<DbPool>,
) {
    info!(
        "matchmaker: started (advertise {}:{}, max {} matches)",
        config.advertise_host, config.udp_port, registry.max_matches
    );

    // A single ticket held while it waits for an opponent to pair with.
    let mut waiting: Option<TicketRequest> = None;
    loop {
        // If a ticket is already waiting, race the next ticket against a fallback
        // timer; otherwise just block for the next ticket.
        let next = if waiting.is_some() {
            tokio::select! {
                r = rx.recv() => r,
                _ = tokio::time::sleep(Duration::from_secs(config.solo_fallback_secs)) => {
                    let lone = waiting.take().expect("waiting is some");
                    info!("matchmaker: no opponent for ticket {} — solo fallback (vs bot)", lone.ticket_id);
                    resolve(&registry, &config, &db, &[lone], 1).await;
                    continue;
                }
            }
        } else {
            rx.recv().await
        };
        let Some(req) = next else { break };

        info!("matchmaker: ticket {} (user {})", req.ticket_id, req.user_id);
        record_match_queued(&db, req.ticket_id, req.user_id).await;
        // Push the captured 3-frame progression's first two frames now; the
        // `Succeeded` frame follows once the match resolves (pair or fallback).
        let _ = req
            .rms
            .send(MatchmakingMessage::Searching { ticket_id: req.ticket_id });
        let _ = req
            .rms
            .send(MatchmakingMessage::PotentialMatch { ticket_id: req.ticket_id });

        match waiting.take() {
            // A second player arrived → pair the two into ONE shared match (no bot).
            Some(first) => resolve(&registry, &config, &db, &[first, req], 0).await,
            // First player → hold it and wait for an opponent (or the timer above).
            None => waiting = Some(req),
        }
    }
    warn!("matchmaker: queue closed, actor exiting");
}

/// Allocate ONE match for these tickets (1 = solo/bot, 2 = a PvP pair) and push
/// `MatchmakingSucceeded` to each — all sharing one `gameSessionId`, each with its
/// own `playerSessionId` (the id the UDP layer admits it under).
async fn resolve(
    registry: &MatchRegistry,
    config: &ArenaConfig,
    db: &Option<DbPool>,
    tickets: &[TicketRequest],
    bots: usize,
) {
    let game_session_id = Uuid::new_v4();
    let paired = tickets.len() >= 2;
    // playerSessionId shape (retail GameLift, capture-confirmed s506
    // `psess-0a7c4b72-0a1c-b2c9-6599-05c28c5ed98e`): the first three UUID groups are
    // DERIVED FROM the shared `gameSessionId`, so paired players' psess share a common
    // `psess-<gsid g1>-<gsid g2>-<gsid g3>-…` prefix, and only the last two groups (the
    // per-player suffix) differ. We previously minted a fully-independent `psess-<new
    // uuid>` per player, so paired players shared no prefix — a divergence from retail
    // that any server-side gsid↔psess correlation (e.g. session lookup) would miss.
    // [docs/arena-journey-log.md §7]
    let psids: Vec<String> = derive_player_session_ids(game_session_id, tickets.len());

    // Each player's loadout (name/UUID for the round-start op50 spawn + combat stats)
    // is loaded here, but BOUNDED by a short timeout per player: awaiting an unbounded
    // `characters` query inline once stalled the single matchmaker actor and hung ALL
    // matchmaking (regression 2026-06-16). On timeout we degrade to the starter loadout
    // so a slow query never hangs matchmaking. (Low-volume today; if this becomes hot,
    // move to a spawned task that injects the loadout before match-start, or a cache.)
    let mut loadouts: Vec<crate::arena::combat::Loadout> = Vec::with_capacity(tickets.len() + bots);
    for t in tickets {
        let lo = match tokio::time::timeout(
            std::time::Duration::from_millis(1500),
            load_loadout(db, t.user_id),
        )
        .await
        {
            Ok(lo) => lo,
            Err(_) => {
                warn!("matchmaker: loadout load timed out (user {}) — starter", t.user_id);
                crate::arena::combat::loadout::starter()
            }
        };
        loadouts.push(lo);
    }

    // DEBUG GHOST (`ARENA_DEBUG_GHOST`): in the solo-fallback path (bots >= 1) the
    // bot fighter(s) otherwise fall back to `loadout::starter()`, whose
    // `profile_character_json` is EMPTY → the engine's `broadcast_profiles` skips it
    // → the client never receives the opponent's op54 PROFILE (GameMessageId 35) →
    // `ClientChecklist.OpponentLoadoutReady` never flips → "Connecting…" forever.
    // When a ghost user_id is configured, load THAT real character into the bot
    // slot(s) so the 2nd fighter has a NON-EMPTY profile and the existing emit path
    // broadcasts the full opponent burst (spawns + op54 PROFILE + stat/state +
    // channeling). Capture-proven fix; see docs/arena-ghost-gap-analysis.md. Each
    // load is bounded by the same 1.5s timeout (a slow query must never hang the
    // single matchmaker actor — regression 2026-06-16). No-op when unset / not a
    // solo-fallback (bots == 0) → today's empty-starter bot.
    //
    // SELF-MATCH GUARD (the 2026-06-19 gate): the opponent ACTOR never instantiates
    // — `PvpEncounter.SpawnOpponent`/`OnOpponentLoaded` never fires + the
    // `ClientChecklist` never advances — when the ghost is the **same character** as
    // the lone human. The client links each Avatar net-object to its Player by the
    // character UUID (the op50 spawn `p4`, identical for self+ghost when both load the
    // same row), so an opponent whose `CharacterUID` equals the local player's can't be
    // built as a *distinct* actor → it collapses onto the local one and the match hangs
    // at "Connecting" even though both players' resources load (frida-confirmed:
    // OnPlayerResourceLoaded ×2, OnOpponentLoaded never). It is NOT a missing relayed
    // user-message — retail sends no s2c GMID 22/36 (capture-proven from s506). This is
    // the documented "self-match spins forever" mode (memory: emulator_character_swap).
    // So if the configured ghost would load the SAME character UUID as the lone human,
    // SKIP it (loud warn) rather than ship a known-broken self-match — point
    // `ARENA_DEBUG_GHOST` at a DIFFERENT character (e.g. Taheen, CharacterUID
    // 33e66455…, retail s506's actual opponent "Blank"). Compares the loaded
    // `character_uuid` (= the row id), so it catches the user-id collision AND any two
    // distinct users that resolve to the same character row.
    if bots > 0 {
        if let Some(ghost_id) = config.debug_ghost_user_id {
            // The lone human's character UUID (slot 0), to reject a self-match ghost.
            // (Index, not `.first()`: diesel's `QueryDsl` is in scope and shadows the
            // slice method on `Vec`.)
            let human_char_uuid: Option<String> =
                loadouts.get(0).map(|l| l.character_uuid.clone());
            for i in 0..bots {
                let lo = match tokio::time::timeout(
                    std::time::Duration::from_millis(1500),
                    load_loadout(db, ghost_id),
                )
                .await
                {
                    Ok(lo) => lo,
                    Err(_) => {
                        warn!("matchmaker: DEBUG ghost loadout load timed out (user {ghost_id}) — starter");
                        crate::arena::combat::loadout::starter()
                    }
                };
                // Reject a ghost that is the SAME character as the human (self-match):
                // a non-empty char UUID that equals slot 0's → the client can't build a
                // distinct opponent actor and hangs at "Connecting". Skip it loudly.
                if let Some(human) = &human_char_uuid {
                    if is_self_match(human, &lo.character_uuid) {
                        warn!(
                            "matchmaker: DEBUG ghost SELF-MATCH rejected — ghost user {ghost_id} \
                             resolves to the SAME character ({}) as the lone human (\"{}\"). The \
                             opponent actor would never instantiate (OnOpponentLoaded never fires); \
                             point ARENA_DEBUG_GHOST at a DIFFERENT character. Slot {} left as the \
                             empty-starter bot.",
                            lo.character_uuid,
                            lo.display_name,
                            tickets.len() + i,
                        );
                        continue;
                    }
                }
                info!(
                    "matchmaker: DEBUG ghost — injected bot slot {} loadout for user {ghost_id} \
                     (\"{}\", char {}, profile_character_json {} B → opponent op54 PROFILE will broadcast)",
                    tickets.len() + i,
                    lo.display_name,
                    lo.character_uuid,
                    lo.profile_character_json.len()
                );
                loadouts.push(lo);
            }
        }
    }

    // APPEARANCE GUARD (docs/arena-appearance-bug-spec.md). Log each fighter's
    // binding UUID at allocation — the client binds opponent appearance by the
    // avatar's propId4 = this `character_uuid`, so distinctness here is what keeps the
    // two avatars from collapsing onto one PvpPlayer (the appearance-swap bug). Logged
    // for EVERY match (paired or bot) so a collision is visible on the wire during
    // bring-up (the spec's verification path).
    let uuids: Vec<&str> = loadouts.iter().map(|l| l.character_uuid.as_str()).collect();
    info!(
        "matchmaker: allocating gsid {game_session_id} — loadouts[*].character_uuid = {uuids:?} \
         ({} fighter(s): {} player(s) + {bots} bot(s))",
        loadouts.len(),
        tickets.len(),
    );

    // For a REAL PAIRED (human-vs-human) match, refuse to ship a known-collapsed
    // appearance: two fighters with the same — or an empty — `character_uuid` make
    // every per-peer opponent-avatar `propId4` equal the local avatar's, so the client
    // dresses the opponent body in the LOCAL char's customization (names stay correct).
    // Mirror the ghost path's `is_self_match` skip, but for the human pair: drop the
    // match rather than ship the swap (the two devices can't be visually distinguished).
    // Bots are excluded (`bots == 0` on the paired path; the solo-vs-bot collapse is
    // the ARENA_DEBUG_GHOST guard's concern). [Fix 1 + Fix 2 of the spec.]
    if paired && bots == 0 {
        if let Err(reason) = check_paired_uuids_distinct(&loadouts) {
            warn!(
                "matchmaker: PAIRED-MATCH APPEARANCE COLLAPSE rejected (gsid {game_session_id}) — {reason} \
                 Refusing to allocate: the client would render both fighters with one appearance \
                 (the avatar→PvpPlayer UUID binding collapses). Tickets left unresolved; re-check \
                 that the two peers resolve to DIFFERENT characters rows (and that load_loadout did \
                 not time out → empty-UUID starter)."
            );
            for t in tickets {
                warn!(
                    "matchmaker: ticket {} unresolved (paired-match appearance guard)",
                    t.ticket_id
                );
            }
            return;
        }
    }

    if !registry.allocate_with_bots(&psids, loadouts, game_session_id, bots) {
        for t in tickets {
            warn!(
                "matchmaker: at capacity — ticket {} left unresolved",
                t.ticket_id
            );
        }
        return;
    }
    info!(
        "matchmaker: resolved {} ({} player(s), gsid {game_session_id}) — clients dial {}:{}",
        if paired { "PAIR" } else { "solo/bot" },
        tickets.len(),
        config.advertise_host,
        config.udp_port
    );

    for (t, psid) in tickets.iter().zip(psids.iter()) {
        let succeeded = MatchmakingMessage::Succeeded {
            ticket_id: t.ticket_id,
            player_session_id: psid.clone(),
            game_session_id,
            address: config.advertise_host.clone(),
            port: config.udp_port,
        };
        if t.rms.send(succeeded).is_err() {
            // The match's capacity permit is held until both players connect; an
            // abandoned ticket leaks one slot until expiry (TODO: deadline sweep).
            warn!(
                "matchmaker: ticket {} — client RMS gone before Succeeded",
                t.ticket_id
            );
        }
        // Record the resolution so the web /arena page can show "matched".
        record_match_resolved(db, t.ticket_id, game_session_id, paired).await;
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // accepted for wire-compat; not used by the v1 solo+bot matcher
pub struct CreateMatchRequest {
    #[serde(default)]
    player_id: Option<Uuid>,
    #[serde(default)]
    fleet_key: Option<String>,
    #[serde(default)]
    player_region_latencies: Option<serde_json::Value>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CreateMatchResponse {
    r#match: MatchTicket,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct MatchTicket {
    ticket_id: Uuid,
    status: &'static str,
    port: u16,
}

#[post("/blades.bgs.services/api/matchmaking/v1/public/matches/create")]
pub async fn create_match(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    _body: web::Json<CreateMatchRequest>,
) -> Result<Json<CreateMatchResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let ticket_id = Uuid::new_v4();

    // The RMS WebSocket must already be open — the client holds it from login.
    // Clone the sender out under a brief lock, then drop the guard.
    let rms = {
        let guard = session.session.matchmaking_ws.lock().await;
        guard.clone()
    }
    .ok_or_else(|| BladeApiError::new(StatusCode::CONFLICT, 4, 1))?;

    app_state
        .arena
        .matchmaker_tx
        .send(TicketRequest {
            ticket_id,
            user_id: session.session.user_id,
            rms,
        })
        .map_err(|_| BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, 4, 2))?;

    Ok(Json(CreateMatchResponse {
        r#match: MatchTicket {
            ticket_id,
            status: "QUEUED",
            port: 0,
        },
    }))
}

#[post("/blades.bgs.services/api/matchmaking/v1/public/matches/{ticket_id}/cancel")]
pub async fn cancel_match(
    path: web::Path<Uuid>,
    session: SessionLookedUpMaybe,
) -> Result<HttpResponse, BladeApiError> {
    let _session = session.get_session_or_error()?;
    info!("matchmaker: cancel ticket {}", path.into_inner());
    // Captured behavior: 200 with a literal `null` body. v1 resolves tickets
    // immediately, so cancellation is an acknowledged no-op.
    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body("null"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::config::ArenaConfig;
    use tokio::sync::mpsc::unbounded_channel;

    /// playerSessionId derivation (Gap 3 + retail s506 shape): all players in a match
    /// share the gameSessionId-derived first-three-group prefix, differ only in the
    /// per-player suffix, and each is a well-formed `psess-`+UUID. Pure (no DB / actor),
    /// so it covers the psess contract independently of the matchmaker loop (which, for a
    /// real PAIR, now needs distinct non-empty character UUIDs — see
    /// `pairs_two_distinct_tickets` / the appearance guard).
    #[test]
    fn psess_derived_from_gsid() {
        let gsid = Uuid::new_v4();
        let psids = derive_player_session_ids(gsid, 2);
        assert_eq!(psids.len(), 2);
        let (psid_a, psid_b) = (&psids[0], &psids[1]);
        assert_ne!(psid_a, psid_b, "each player gets a distinct playerSessionId");

        let gsid_s = gsid.to_string();
        let want_prefix =
            format!("psess-{}", gsid_s.splitn(4, '-').take(3).collect::<Vec<_>>().join("-"));
        assert!(
            psid_a.starts_with(&want_prefix) && psid_b.starts_with(&want_prefix),
            "both psess derive their first 3 groups from the gsid: prefix {want_prefix}, got {psid_a} / {psid_b}"
        );
        for (label, psid) in [("A", psid_a), ("B", psid_b)] {
            let body = psid.strip_prefix("psess-").expect("psess- prefix");
            assert_eq!(
                body.split('-').count(),
                5,
                "psess {label} is a well-formed UUID body (8-4-4-4-12): {psid}"
            );
        }
        let suffix = |p: &str| p.splitn(4, '-').skip(3).collect::<Vec<_>>().join("-");
        assert_ne!(suffix(psid_a), suffix(psid_b), "per-player suffixes are distinct");
    }

    /// Two tickets enqueued back-to-back form ONE shared match — but a DB-less pair
    /// (both `load_loadout`s fall back to the empty-UUID `starter()`) is now REFUSED by
    /// the paired-match appearance guard (`docs/arena-appearance-bug-spec.md`): two
    /// empty `character_uuid`s would collapse both avatars onto the local PvpPlayer
    /// (the appearance-swap bug). So no `Succeeded` is sent and the capacity permit is
    /// returned. (In production each real user resolves to a DISTINCT characters row →
    /// distinct non-empty UUIDs → the pair is allocated; the gsid/psess shape itself is
    /// covered by `psess_derived_from_gsid`.)
    #[tokio::test]
    async fn pairs_two_tickets_refused_when_uuids_collapse() {
        let registry = MatchRegistry::new(4);
        let config = ArenaConfig {
            advertise_host: "127.0.0.1".into(),
            udp_port: 7777,
            max_concurrent_matches: 4,
            max_queued_players: 64,
            solo_fallback_secs: 15,
            debug_ghost_user_id: None,
        };
        let (tx, rx) = unbounded_channel::<TicketRequest>();
        tokio::spawn(matchmaker_loop(rx, config, registry.clone(), None));

        let (rms_a, mut recv_a) = unbounded_channel();
        let (rms_b, mut recv_b) = unbounded_channel();
        tx.send(TicketRequest {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            rms: rms_a,
        })
        .unwrap();
        tx.send(TicketRequest {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            rms: rms_b,
        })
        .unwrap();

        // No `Succeeded` arrives on either channel (the empty-UUID pair is refused).
        let got_a = tokio::time::timeout(Duration::from_millis(500), recv_a.recv()).await;
        let got_b = tokio::time::timeout(Duration::from_millis(500), recv_b.recv()).await;
        let no_succeeded = |r: &Result<Option<MatchmakingMessage>, _>| {
            !matches!(r, Ok(Some(MatchmakingMessage::Succeeded { .. })))
        };
        assert!(no_succeeded(&got_a), "no Succeeded for an empty-UUID paired match (appearance guard)");
        assert!(no_succeeded(&got_b), "no Succeeded for an empty-UUID paired match (appearance guard)");
        // The capacity permit was returned (no match allocated), so all 4 are free.
        assert_eq!(registry.available_permits(), 4, "the refused pair holds no capacity permit");
    }

    /// The DEBUG-ghost self-match guard: a ghost that resolves to the SAME
    /// character UUID as the lone human is rejected (the opponent actor would never
    /// instantiate on the client → permanent "Connecting"); a DIFFERENT character is
    /// accepted; and an empty UUID (starter loadout) is never treated as a self-match.
    #[test]
    fn ghost_self_match_is_detected() {
        let human = "3ef856f9-a624-400a-81f4-0bb3f7238b34"; // WolfWalker (the emu's char)
        // Same character (the 2026-06-19 self-match bug: ghost == bound human char).
        assert!(is_self_match(human, human), "same char UUID ⇒ self-match");
        // A distinct opponent (e.g. Taheen) is fine.
        assert!(
            !is_self_match(human, "e0939d05-fc71-5f5e-a79d-fd1cb465efcb"),
            "different char UUID ⇒ not a self-match"
        );
        // An empty ghost UUID (starter loadout / no character) is never a self-match,
        // even against an empty human UUID — don't reject the legitimate bot fallback.
        assert!(!is_self_match(human, ""), "empty ghost UUID ⇒ not a self-match");
        assert!(!is_self_match("", ""), "two empty UUIDs ⇒ not a self-match");
    }

    /// The PAIRED-match appearance guard (docs/arena-appearance-bug-spec.md): two
    /// real fighters must have DISTINCT, non-empty `character_uuid`s, or the client's
    /// avatar→PvpPlayer binding collapses both avatars onto the local player (the
    /// appearance-swap bug; names stay correct). `check_paired_uuids_distinct` accepts
    /// distinct non-empty UUIDs and rejects (a) two equal UUIDs and (b) any empty UUID.
    #[test]
    fn paired_uuid_distinctness_guard() {
        use crate::arena::combat::loadout::starter;
        let with_uuid = |uuid: &str, name: &str| {
            let mut l = starter();
            l.character_uuid = uuid.to_string();
            l.display_name = name.to_string();
            l
        };

        // Distinct, non-empty UUIDs → OK (the WolfWalker-vs-Blank happy path).
        let ok = vec![
            with_uuid("38c987fd-c42b-4ea6-b869-c8d4c03055f9", "Flappety"),
            with_uuid("1131a037-716c-49cc-b165-32d8ddc14f49", "Blank"),
        ];
        assert!(check_paired_uuids_distinct(&ok).is_ok(), "distinct non-empty UUIDs must pass");

        // Two equal non-empty UUIDs → rejected (both peers resolved to the same row →
        // appearance collapse). This is the WolfWalker-vs-Flappety reported symptom.
        let same = vec![
            with_uuid("38c987fd-c42b-4ea6-b869-c8d4c03055f9", "WolfWalker"),
            with_uuid("38c987fd-c42b-4ea6-b869-c8d4c03055f9", "Flappety"),
        ];
        let err = check_paired_uuids_distinct(&same).expect_err("shared UUID must be rejected");
        assert!(err.contains("share character_uuid"), "rejection names the shared-UUID collapse: {err}");

        // An empty UUID (a starter() fallback on a slow load_loadout) → rejected: its
        // avatar propId4 would be "" → can't bind a distinct opponent, drops the profile.
        let empty = vec![
            with_uuid("38c987fd-c42b-4ea6-b869-c8d4c03055f9", "Flappety"),
            with_uuid("", "DegradedToStarter"),
        ];
        let err = check_paired_uuids_distinct(&empty).expect_err("empty UUID must be rejected");
        assert!(err.contains("EMPTY character_uuid"), "rejection names the empty-UUID collapse: {err}");
    }

    /// The op54 round-start PROFILE character JSON must be schema-identical to
    /// retail: for a character WITH a `challenge_season` and a non-empty `data`
    /// (customization + dialog + new-flags), the built JSON must have NO
    /// top-level `challengeSeason` key, and its `data` must contain ONLY
    /// `customization` (no `dialog`, no `new-flags`) — with the customization
    /// (the avatar appearance / CharacterUID) preserved verbatim. Capture-proven
    /// by the field-diff of session 506.
    #[test]
    fn profile_character_json_matches_retail_schema() {
        use blades_lib::user_data::{
            CharacterChallengeSeason, CompleteCharacter, CompleteCharacterData,
        };
        use serde_json::json;

        // A leveled character WITH a (non-default) challenge_season + the
        // `completed_quests` / `global_shop_offers` fields populated — exactly the
        // top-level keys our profile used to over-emit but retail's profile never
        // carries (capture-proven: 0/830 retail op54 profile frames have them).
        let mut character = CompleteCharacter::default();
        character.name = "Opponent".into();
        character.level = 86;
        character.completed_quests = json!({ "q1": { "completed": true }, "q2": { "completed": true } });
        character.global_shop_offers = json!([{ "offerId": "x", "price": 100 }]);
        character.challenge_season = CharacterChallengeSeason {
            current_session_id: Uuid::new_v4(),
            rank: 7,
            rank_rewarded: 3,
            points: 1234,
            season_year: 2026,
            premium: true,
        };

        // A non-empty `data`: customization (with a CharacterUID) + dialog +
        // new-flags — exactly the keys our profile used to over-emit.
        let customization = json!({
            "CharacterUID": "11111111-2222-3333-4444-555555555555",
            "appearance": { "hair": 3, "skinTone": 7 }
        });
        let data = CompleteCharacterData {
            customization: customization.clone(),
            new_flags: json!({ "seenTutorial": true }),
            dialog: json!({ "npc_a": { "stage": 4 } }),
        };

        let id = Uuid::new_v4();
        let out = build_profile_character_json(&data, id, &character);
        let v: serde_json::Value =
            serde_json::from_str(&out).expect("profile character JSON must parse");
        let obj = v.as_object().expect("profile is a JSON object");

        // No top-level `challengeSeason`, `completedQuests`, or `globalShopOffers`
        // (retail's profile carries none of the three — capture-proven from s506).
        for forbidden in ["challengeSeason", "completedQuests", "globalShopOffers"] {
            assert!(
                !obj.contains_key(forbidden),
                "{forbidden} must be trimmed from the op54 profile; got keys: {:?}",
                obj.keys().collect::<Vec<_>>()
            );
        }

        // `data` is customization-ONLY.
        let data_obj = obj
            .get("data")
            .and_then(|d| d.as_object())
            .expect("`data` must be a JSON object");
        let data_keys: Vec<&String> = data_obj.keys().collect();
        assert_eq!(
            data_keys,
            vec![&"customization".to_string()],
            "`data` must contain ONLY `customization`; got {:?}",
            data_keys
        );
        assert!(!data_obj.contains_key("dialog"), "`dialog` must be dropped");
        assert!(
            !data_obj.contains_key("new-flags"),
            "`new-flags` must be dropped"
        );

        // customization (avatar appearance / CharacterUID) preserved VERBATIM.
        assert_eq!(
            data_obj.get("customization"),
            Some(&customization),
            "customization must be preserved verbatim"
        );

        // Sanity: the rest of the profile still serialized (id + a real field).
        assert_eq!(obj.get("id").and_then(|i| i.as_str()), Some(id.to_string().as_str()));
        assert_eq!(obj.get("name").and_then(|n| n.as_str()), Some("Opponent"));
    }
}
