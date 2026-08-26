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
use std::time::{Duration, Instant};

use actix_web::{
    HttpResponse,
    http::StatusCode,
    post,
    web::{self, Json},
};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::pooled_connection::bb8::PooledConnection;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
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
    session::{Session, SessionLookedUpMaybe},
};

/// How the matchmaker reaches a ticket's client over its RMS WebSocket.
///
/// The RMS sender is NOT captured once at enqueue time. The client reconnects its
/// rms WS repeatedly (each reconnect overwrites `Session.matchmaking_ws`), so a sender
/// cloned at `create_match` time can be a STALE channel the client no longer reads —
/// `Succeeded` sent into it silently vanishes and the client hangs at "determining
/// server" forever (the 2026-07 stale-sender race). Instead we hold the `Arc<Session>`
/// and re-fetch the CURRENT live sender at every send, so a client that reconnected
/// still receives the address.
///
/// `Direct` is the unit-test variant: an `UnboundedSender` with no backing session
/// (the matchmaker tests have no `SessionStore`), behaving as before.
pub enum RmsHandle {
    /// Production: re-fetch `session.matchmaking_ws` live on each access.
    Session(Arc<Session>),
    /// Tests: a fixed sender (no session store to re-fetch from).
    Direct(UnboundedSender<MatchmakingMessage>),
}

impl RmsHandle {
    /// Snapshot the CURRENT live sender (None if the client has no rms WS open right
    /// now). For `Session` this reads `matchmaking_ws` under its async lock, so a
    /// reconnect since enqueue is picked up.
    async fn current(&self) -> Option<UnboundedSender<MatchmakingMessage>> {
        match self {
            RmsHandle::Session(s) => s.matchmaking_ws.lock().await.clone(),
            RmsHandle::Direct(tx) => Some(tx.clone()),
        }
    }

    /// True iff the client currently has NO live rms sender (never connected, or the
    /// sender is closed because its WS reader task exited). Used to skip resolving a
    /// ticket whose client is gone.
    async fn is_gone(&self) -> bool {
        match self.current().await {
            Some(tx) => tx.is_closed(),
            None => true,
        }
    }

    /// Send `msg` to the client's CURRENT live sender. `Ok(())` on success; `Err(())`
    /// when there is no live sender or the send failed (logged by the caller).
    async fn send(&self, msg: MatchmakingMessage) -> Result<(), ()> {
        match self.current().await {
            Some(tx) => tx.send(msg).map_err(|_| ()),
            None => Err(()),
        }
    }
}

/// A command handed to the matchmaker actor over its single channel. Cancellation is
/// routed through the SAME channel as enqueue so the actor (the sole owner of the
/// `waiting` slot) can actually DEQUEUE a cancelled ticket — a `cancel` handler can't
/// touch `waiting` directly (no shared lock), which is why the old cancel was a no-op
/// and a cancelled ticket still zombie-resolved on the fallback timer.
pub enum MatchmakerCommand {
    /// Enqueue a new ticket.
    Enqueue(TicketRequest),
    /// Remove a ticket from the queue (client cancelled) — never zombie-resolves.
    Cancel { ticket_id: Uuid, user_id: Uuid },
}

/// A queued matchmaking ticket handed to the matchmaker actor. Carries an [`RmsHandle`]
/// so the matchmaker re-fetches the requesting client's CURRENT rms sender at send time
/// (surviving rms-WS reconnects), rather than a stale sender captured at enqueue.
pub struct TicketRequest {
    pub ticket_id: Uuid,
    pub user_id: Uuid,
    /// The character this player is queueing AS.
    ///
    /// The client tells us, in `matches/create`'s `playerId` — capture-confirmed: that
    /// UUID appears 3,198 times in `/characters/{id}/...` paths in the corpus and zero
    /// times where an account id belongs. We used to bind the request body to `_body`
    /// and throw it away, then load `characters WHERE user_id = ?` and take whatever row
    /// Postgres happened to return first. An account with more than one character
    /// therefore fought as an ARBITRARY one — wrong gear, wrong name, wrong stats — and
    /// the choice could change between queries. Reported after the first human-vs-human
    /// match: "I changed to dwarven mail and frost, but it looked like I was fighting
    /// flappety in dragon bone... I may have fought you in my equipment too."
    ///
    /// `None` only when the client omitted it; the loader then falls back to the
    /// player's strongest character, which is at least deterministic and agrees with
    /// [`load_skill`].
    pub character_id: Option<Uuid>,
    pub rms: RmsHandle,
    /// What we know about this player's strength, for the pairing bracket.
    /// `None` when the lookup failed or the player has no character yet — an
    /// unknown player is never blocked from matching, only from being used as a
    /// reason to block someone else.
    pub skill: Option<Skill>,
}

/// A queued player's strength, read once at enqueue.
///
/// WHY THIS EXISTS (tracker #19)
///
/// Matchmaking held ONE waiting ticket and paired it with whoever queued next,
/// with no regard for how strong either player was. Taheen, level 43, was matched
/// against a level 66 and lost 0-2 in under a minute. It is also why his "the
/// damage feels off" could not be judged: a 23-level gap swamps any question about
/// damage numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Skill {
    pub level: i32,
    /// `character.matchmakingPvpTrophies` — retail's own matchmaking rating, which
    /// is why it is preferred over level as the primary signal.
    pub trophies: i64,
}

/// How far apart two players may be, given how long the LONGER-WAITING of them has
/// been queued. Widens in steps, then gives up and allows anyone.
///
/// The numbers are chosen against our own data rather than invented: recorded
/// arena trophies span roughly 30-850 and levels 1-100, and the pairing that
/// prompted this was 23 levels apart. A 5-level opening bracket refuses that
/// immediately; by 30 seconds anyone will do, because on a server with a handful
/// of players a perfect match is worth less than a match.
///
/// Returns `(max level gap, max trophy gap)`. `None` means unlimited.
pub fn bracket_for(waited: Duration) -> Option<(i32, i64)> {
    match waited.as_secs() {
        0..=9 => Some(BRACKET_STEPS[0]),
        10..=19 => Some(BRACKET_STEPS[1]),
        20..=29 => Some(BRACKET_STEPS[2]),
        _ => None,
    }
}

/// The bounded widening steps of the bracket, TIGHTEST FIRST — `(max level gap,
/// max trophy gap)`.
///
/// [`bracket_for`] walks these by how long a human has waited. [`pick_bot_index`]
/// walks the same table by *preference*, because a bot draw has no arrival time to
/// wait out: the whole candidate pool is visible at once, so it takes the tightest
/// step that has anybody in it. Sharing one table is the point — a bot must not be
/// allowed to be a worse match than a human would have been.
pub const BRACKET_STEPS: [(i32, i64); 3] = [(5, 150), (10, 300), (20, 600)];

/// Are these two inside one specific bracket step? The single level/trophy
/// comparison in this module — [`compatible`] and [`pick_bot_index`] both go
/// through it so the human and bot paths cannot drift apart.
fn within_step(a: Skill, b: Skill, step: (i32, i64)) -> bool {
    (a.level - b.level).abs() <= step.0 && (a.trophies - b.trophies).abs() <= step.1
}

/// May these two be paired right now?
///
/// An unknown skill on either side returns true: a failed lookup must not strand a
/// player in the queue forever. The bracket is a preference we enforce while we
/// can, not a gate that can deadlock the arena.
pub fn compatible(a: Option<Skill>, b: Option<Skill>, waited: Duration) -> bool {
    let Some(step) = bracket_for(waited) else {
        return true;
    };
    match (a, b) {
        (Some(x), Some(y)) => within_step(x, y, step),
        _ => true,
    }
}

/// At the fallback deadline, pick a human to pair with instead of a bot — **ignoring
/// the bracket**.
///
/// The bracket ([`bracket_for`]) is a preference, not a gate, and it was silently
/// outliving the thing it shares a queue with. `solo_fallback_secs` is 4 s; the first
/// bracket step lasts 10 s. So two players outside [`BRACKET_STEPS`]`[0]` could sit in
/// `waiting` *together* and both be handed a bot before the bracket ever widened once —
/// the later steps were unreachable in a two-player arena. Observed with two testers
/// pressing Fight seconds apart and never meeting.
///
/// The deadline is the moment the choice stops being "good match vs. better match" and
/// becomes "this human vs. a bot". A lopsided human fight beats a bot, so at last call
/// the bracket is dropped entirely.
///
/// Among those present it still takes the CLOSEST in trophies, tie-broken by longest
/// waiting — the same ordering as the in-bracket path.
///
/// Returns the index into `candidates`, or `None` if nobody else is waiting.
fn last_call_partner(
    candidates: &[(Option<Skill>, Instant)],
    lone: Option<Skill>,
    now: Instant,
) -> Option<usize> {
    let mut best: Option<(usize, i64, Duration)> = None;
    for (i, (skill, since)) in candidates.iter().enumerate() {
        let waited = now.saturating_duration_since(*since);
        let gap = match (skill, lone) {
            (Some(a), Some(b)) => (a.trophies - b.trophies).abs(),
            _ => i64::MAX,
        };
        let better = match best {
            None => true,
            Some((_, best_gap, best_waited)) => {
                gap < best_gap || (gap == best_gap && waited > best_waited)
            }
        };
        if better {
            best = Some((i, gap, waited));
        }
    }
    best.map(|(i, _, _)| i)
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
async fn load_loadout(
    db: &Option<DbPool>,
    user_id: Uuid,
    character_id: Option<Uuid>,
) -> crate::arena::combat::Loadout {
    use crate::arena::combat::loadout;
    let Some(db) = db else {
        return loadout::starter();
    };
    let Ok(mut conn) = db.get().await else {
        return loadout::starter();
    };
    // ALWAYS scoped by user_id, even when the client named a character: `playerId`
    // arrives from the client and is not trustworthy on its own. Filtering on both means
    // a forged id selects nothing and we fall back, rather than loading somebody else's
    // character into the arena.
    let rows = characters::table
        .filter(characters::user_id.eq(user_id))
        .select(CharacterDbEntryCharacterWalletInventory::as_select())
        .load(&mut conn)
        .await
        .ok()
        .unwrap_or_default();
    let row = pick_character(rows, character_id);
    match row {
        Some(r) => loadout_from_row(&r),
        None => loadout::starter(),
    }
}

/// Build a full combat [`Loadout`] from a loaded `characters` row: the parsed combat
/// stats PLUS the identity the round-start emit needs — `character_uuid` (op50 spawn
/// `p4` / avatar propId4) and the op54 PROFILE JSON. Shared by [`load_loadout`] (the
/// human) and [`load_bot_loadout`] (the solo bot), so a bot gets the SAME non-empty
/// profile a human does — the op54 PROFILE (GameMessageId 35) is the frame that makes
/// the opponent VISIBLE and bindable. An empty starter profile → invisible/unkillable
/// bot + a match-end hang (the 2026-07-03 solo-bot bug).
///
/// The profile MUST include `data.customization` (the opponent's avatar visual) or the
/// client's resource-load hangs at "Connecting"; `build_profile_character_json` also
/// trims it to retail's exact schema (dropping keys retail never sends, which the
/// client's deserializer would reject). `equippedItems` now carries retail's per-item
/// `grade` and `arcaneTier` (`Item`), both omitted when absent so an item that has
/// neither serializes byte-identically to before they were modelled.
/// Choose which of the player's characters enters the arena.
///
/// Named character wins. Otherwise the STRONGEST by (trophies, level) — the same
/// ordering [`load_skill`] uses, so the character you are bracketed as is the character
/// you actually fight as. Previously these disagreed: skill took the max, the loadout
/// took an arbitrary row, so a player could be matched against opponents chosen for
/// their best character while fighting as their worst.
fn pick_character(
    rows: Vec<CharacterDbEntryCharacterWalletInventory>,
    character_id: Option<Uuid>,
) -> Option<CharacterDbEntryCharacterWalletInventory> {
    if let Some(want) = character_id {
        if let Some(hit) = rows.iter().position(|r| r.id == want) {
            return rows.into_iter().nth(hit);
        }
        // Fall through: the id named a character this user does not own, or one that has
        // since been deleted. Better a deterministic fallback than no match at all.
    }
    rows.into_iter()
        .max_by_key(|r| (r.character.0.matchmaking_pvp_trophies, r.character.0.level))
}

fn loadout_from_row(r: &CharacterDbEntryCharacterWalletInventory) -> crate::arena::combat::Loadout {
    use crate::arena::combat::loadout;
    let mut lo = loadout::from_character(&r.character.0, &r.inventory.0);
    lo.character_uuid = r.id.to_string();
    lo.profile_equipped_json =
        serde_json::json!({ "equippedItems": &r.inventory.0.loadout.equipped_items }).to_string();
    lo.profile_character_json = build_profile_character_json(&r.data.0, r.id, &r.character.0);

    // DIAGNOSTIC for "no ability buttons in a match", reported 2026-08-01 by two
    // players (Taheen, Swanne) while a third (Flappety) is unaffected.
    //
    // Ruled out from the stored data already: all three have 6 equipped abilities
    // with identical slot-keyed shape, no equipped ability is missing from the
    // owned map, loadout profiles / inventory / customization are structurally the
    // same, ability RANKS are not out of range, and every one of the 13 distinct
    // equipped UUIDs is known to both this server and reference/game-defs.
    //
    // So the difference is not the character row. The remaining candidates are all
    // per-match and need a live match to distinguish: how many abilities survive
    // into the Loadout, and how big the profile is — retail's op54 profile is
    // ~17 KB / 16 ENet fragments, and ours was 31 KB / 26 before trimming, so a
    // player whose profile is unusually large is a real suspect. Log both, per
    // fighter, so the next match by an affected player produces the evidence
    // rather than requiring them to be online while someone watches.
    info!(
        "arena loadout: char {} \"{}\" — equipped_abilities={} profile_json={}B equipped_json={}B",
        r.id,
        lo.display_name,
        lo.abilities.len(),
        lo.profile_character_json.len(),
        lo.profile_equipped_json.len(),
    );

    lo
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
    let serialized =
        match serde_json::to_string(&blades_lib::user_data::CompleteCharacterWithIdAndData {
            data: data.clone(),
            id,
            character: character.clone(),
        }) {
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

/// True iff a loaded `characters` row has a non-empty `data.customization` — the
/// opponent avatar's appearance. A bot WITHOUT it renders nothing and the client's
/// resource-load hangs at "Connecting", so solo-bot selection filters on this.
fn row_has_customization(r: &CharacterDbEntryCharacterWalletInventory) -> bool {
    serde_json::to_value(&r.data.0)
        .ok()
        .and_then(|v| v.get("customization").cloned())
        .and_then(|c| c.as_object().map(|o| !o.is_empty()))
        .unwrap_or(false)
}

/// The strength of a loaded `characters` row, for the bot draw's bracket.
///
/// Reads the SAME two fields `load_skill` reads for a queueing human —
/// `character.level` and `character.matchmakingPvpTrophies` — but off the already
/// loaded row rather than a second query, since `pick_bot_loadout` has the whole
/// pool in hand. `None` is impossible today (both are non-optional on
/// `CompleteCharacter`) and is kept as the shape so a future nullable column
/// degrades to "unknown, don't block" rather than to a level of 0.
fn skill_of_row(r: &CharacterDbEntryCharacterWalletInventory) -> Option<Skill> {
    Some(Skill {
        level: r.character.0.level as i32,
        trophies: r.character.0.matchmaking_pvp_trophies,
    })
}

/// One row of the bot pool, as [`pick_bot_index`] sees it: the candidate's
/// character UUID, whether its profile is COMPLETE enough to render, and its
/// strength (`None` when the row's level/trophies could not be read).
pub type BotCandidate = (String, bool, Option<Skill>);

/// The outcome of a bot draw.
///
/// `step` is the bracket the chosen candidate satisfied, or `None` when no tier
/// had anybody and the draw fell back to "any eligible opponent". Callers need
/// that distinction: a fallback draw is a mismatch we tolerate to start a match
/// at all, and tracker #49 showed it happening silently for months.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BotDraw {
    pub index: usize,
    pub step: Option<(i32, i64)>,
}

/// Pure selection: from the candidate pool, choose a bot that is COMPLETE (renders),
/// distinct from the human (not a self-match), and AS CLOSE TO THE HUMAN'S STRENGTH
/// as the pool allows. Returns the chosen index, or `None` if none qualifies.
///
/// WHY THE STRENGTH TIER EXISTS (tracker #24)
///
/// The 30-second widening bracket at [`bracket_for`] governs HUMAN pairing only.
/// Bots bypassed it entirely: selection filtered on complete + non-self and then
/// rotated by gsid, so the pool's *level* never entered the decision. The reporter
/// is level 43 and drew bots at levels 89, 93, 66 and 89 — every one of his six
/// matches was a solo bot fallback — taking 30-48 % of his health per hit and dying
/// in two or three. tracker #19 had already fixed exactly this for humans; the bot
/// path was simply never wired to the same rule.
///
/// The band is [`BRACKET_STEPS`] — the human bracket's own numbers. We hold NO
/// retail matchmaking capture and no shipped matchmaking table, so there is no
/// retail-derived band to use; the human bracket is the closest in-repo precedent
/// and reusing its table by construction means a bot can never be a worse match
/// than a human would have been allowed to be.
///
/// A bot draw has no arrival time to wait out (the whole pool is visible at once
/// and the human has ALREADY waited out the solo-fallback timer to get here), so
/// the steps are walked as a preference ladder, tightest first, and the first
/// non-empty tier wins. Within a tier the gsid rotation is preserved, so variety
/// across matches survives.
///
/// It is a PREFERENCE, never a gate: if no tier has anybody — or the human's or the
/// candidates' skill could not be read — it falls back to the full eligible set
/// rather than refuse to start a match. An unfair bot beats no opponent at all,
/// which is the same call `compatible` makes for humans.
fn pick_bot_index(
    candidates: &[BotCandidate],
    human_char_uuid: &str,
    human: Option<Skill>,
    gsid: Uuid,
) -> Option<BotDraw> {
    let eligible: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, (uuid, complete, _))| *complete && !is_self_match(human_char_uuid, uuid))
        .map(|(i, _)| i)
        .collect();
    if eligible.is_empty() {
        return None;
    }
    // Rotate by the (random) gsid's first byte → variety across matches, deterministic
    // for a given match. No RNG (Date/rand are unavailable/undesired in this actor).
    let seed = gsid.as_bytes()[0] as usize;
    let rotate = |pool: &[usize]| pool[seed % pool.len()];

    let Some(h) = human else {
        return Some(BotDraw {
            index: rotate(&eligible),
            step: None,
        });
    };
    for step in BRACKET_STEPS {
        let tier: Vec<usize> = eligible
            .iter()
            .copied()
            .filter(|&i| candidates[i].2.is_some_and(|c| within_step(h, c, step)))
            .collect();
        if !tier.is_empty() {
            return Some(BotDraw {
                index: rotate(&tier),
                step: Some(step),
            });
        }
    }
    Some(BotDraw {
        index: rotate(&eligible),
        step: None,
    })
}

/// Load a SOLO-match bot opponent: a real, COMPLETE, distinct character so the bot has a
/// non-empty op54 PROFILE (a visible/bindable/killable opponent + a resolvable match-end
/// card — the 2026-07-03 solo-bot fix). Pool = the configured `ARENA_BOT_USER_IDS`
/// roster if set, else any OTHER character in the DB. Filters to complete + non-self,
/// rotates by gsid. Falls back to the empty `starter()` (logged) only if NOTHING
/// qualifies (e.g. no other complete character exists yet).
/// The suffix a bot opponent's name carries so a player can tell at a glance that
/// they are not fighting a person.
///
/// Requested 2026-08-03. It matters for a reason beyond politeness: a solo match
/// loads a REAL, complete character from the bot roster, so the opponent shows a
/// real player's name, gear and appearance. Without a marker there is no way to
/// know whether you just beat a human or a script — and no way to know whether a
/// loss is worth taking personally.
const BOT_NAME_SUFFIX: &str = " (AI)";

/// What the engine calls a fighter with no name (`engine.rs`, the op50 Player
/// spawn). Duplicated deliberately and named, so the two cannot silently diverge
/// into "Fighter" on one path and " (AI)" on the other.
const BOT_FALLBACK_NAME: &str = "Fighter";

/// Mark a loadout as a bot's, everywhere the client reads a name.
///
/// The name reaches the client TWICE and they must agree, or the HUD and the
/// match-end card disagree about who you fought:
///   * `display_name` — the op50 Player spawn's name field (`engine.rs`), and what
///     `fighter_display_name` feeds into the match-end result card;
///   * `name` inside `profile_character_json` — the op54 PROFILE, which is what the
///     client actually renders for the opponent's plate.
///
/// Idempotent: `load_bot_loadout` has two return paths and a future third would
/// otherwise be able to produce "Blank (AI) (AI)".
fn mark_loadout_as_bot(lo: &mut crate::arena::combat::Loadout) {
    if lo.display_name.is_empty() {
        // The `starter()` fallback carries no name, and the engine substitutes
        // "Fighter" for an empty one when it writes the op50 Player spawn. Appending
        // to "" would make the field non-empty and defeat that, putting a bare
        // " (AI)" on screen — so spell out the same fallback here.
        lo.display_name = format!("{BOT_FALLBACK_NAME}{BOT_NAME_SUFFIX}");
    } else if !lo.display_name.ends_with(BOT_NAME_SUFFIX) {
        lo.display_name.push_str(BOT_NAME_SUFFIX);
    }
    // The profile is JSON we built ourselves a moment ago, so a parse failure here
    // means it was already malformed; leave it alone rather than replacing a broken
    // profile with a differently broken one — an unnamed opponent still fights.
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&lo.profile_character_json) {
        if let Some(name) = v.get_mut("name").and_then(|n| n.as_str().map(String::from)) {
            if !name.ends_with(BOT_NAME_SUFFIX) {
                v["name"] = serde_json::Value::String(format!("{name}{BOT_NAME_SUFFIX}"));
                lo.profile_character_json = v.to_string();
            }
        }
    }
}

/// Load a bot opponent and ALWAYS mark it. A thin wrapper over
/// [`pick_bot_loadout`] for one reason: that function has four return paths
/// (no DB, no connection, a picked row, the starter fallback) and an unmarked one
/// ships a bot wearing a real player's name. Marking here instead of at each
/// return makes an unmarked path impossible to write.
async fn load_bot_loadout(
    db: &Option<DbPool>,
    human_char_uuid: &str,
    human: Option<Skill>,
    config: &ArenaConfig,
    gsid: Uuid,
) -> crate::arena::combat::Loadout {
    let mut lo = pick_bot_loadout(db, human_char_uuid, human, config, gsid).await;
    mark_loadout_as_bot(&mut lo);
    lo
}

async fn pick_bot_loadout(
    db: &Option<DbPool>,
    human_char_uuid: &str,
    human: Option<Skill>,
    config: &ArenaConfig,
    gsid: Uuid,
) -> crate::arena::combat::Loadout {
    use crate::arena::combat::loadout;
    let Some(db) = db else {
        return loadout::starter();
    };
    let Ok(mut conn) = db.get().await else {
        return loadout::starter();
    };

    let use_roster = !config.bot_user_ids.is_empty();
    let mut rows: Vec<CharacterDbEntryCharacterWalletInventory> = if use_roster {
        characters::table
            .filter(characters::user_id.eq_any(config.bot_user_ids.clone()))
            .select(CharacterDbEntryCharacterWalletInventory::as_select())
            .load(&mut conn)
            .await
            .unwrap_or_default()
    } else {
        load_wide_pool(&mut conn, human_char_uuid).await
    };

    let mut candidates: Vec<BotCandidate> = rows.iter().map(candidate_of_row).collect();
    let mut draw = pick_bot_index(&candidates, human_char_uuid, human, gsid);

    // THE ROSTER IS A PREFERENCE, NOT A CAGE (tracker #49).
    //
    // A curated `ARENA_BOT_USER_IDS` roster keeps opponents complete and
    // presentable, but it is small. When nobody in it is inside even the widest
    // bracket step, honouring the roster means handing the player a wildly
    // mismatched fight — the reported case was a level 43 / 49-trophy player drawn
    // against a level 68 / 790-trophy bot, 25 levels and 741 trophies above him,
    // while three characters within the TIGHTEST step existed outside the roster.
    //
    // Two preferences collided and the roster silently won. The bracket is the one
    // the player feels, so when the roster cannot satisfy it, widen to the whole
    // character pool and take a bracketed opponent from there. Completeness is not
    // sacrificed: the wider pool goes through the same `row_has_customization`
    // filter, so an unrenderable character still cannot be drawn.
    //
    // Only ever a widening — if the wider pool has nobody bracketed either, the
    // roster draw stands.
    if should_widen(use_roster, human, draw) {
        let wide_rows = load_wide_pool(&mut conn, human_char_uuid).await;
        let wide_candidates: Vec<BotCandidate> = wide_rows.iter().map(candidate_of_row).collect();
        if let Some(d) = pick_bot_index(&wide_candidates, human_char_uuid, human, gsid)
            && d.step.is_some()
        {
            info!(
                "matchmaker: the {}-character bot roster had nobody inside the bracket for \
                     this player — widened to the full character pool ({} candidates) and drew a \
                     bracketed opponent instead",
                candidates.len(),
                wide_candidates.len(),
            );
            rows = wide_rows;
            candidates = wide_candidates;
            draw = Some(d);
        }
    }

    match draw {
        Some(d) => {
            if let (Some(h), Some(b)) = (human, candidates[d.index].2) {
                let (dl, dt) = ((b.level - h.level).abs(), (b.trophies - h.trophies).abs());
                if d.step.is_some() {
                    info!(
                        "matchmaker: bot drawn at level {} / {} trophies vs the player's {} / {} \
                         (level gap {}, trophy gap {}) from a pool of {}",
                        b.level,
                        b.trophies,
                        h.level,
                        h.trophies,
                        dl,
                        dt,
                        candidates.len(),
                    );
                } else {
                    // Previously silent. This is the line that would have explained
                    // tracker #49 the first time it happened instead of months later.
                    warn!(
                        "matchmaker: NO bracketed opponent anywhere for a player at level {} / {} \
                         trophies — drew level {} / {} (level gap {}, trophy gap {}) from a pool \
                         of {}. The widest step is {:?}; this match is a known mismatch. Transfer \
                         a character near this player's strength to fix it.",
                        h.level,
                        h.trophies,
                        b.level,
                        b.trophies,
                        dl,
                        dt,
                        candidates.len(),
                        BRACKET_STEPS[BRACKET_STEPS.len() - 1],
                    );
                }
            }
            loadout_from_row(&rows[d.index])
        }
        None => {
            warn!(
                "matchmaker: no COMPLETE distinct bot character available (pool {}) — bot falls \
                 back to the empty starter (INVISIBLE opponent). Seed ARENA_BOT_USER_IDS or \
                 transfer more characters with appearance.",
                candidates.len()
            );
            loadout::starter()
        }
    }
}

/// Should a roster draw be widened to the whole character pool?
///
/// Split out from [`pick_bot_loadout`] so the decision is testable without a
/// database — the bug in tracker #49 was entirely in this condition being absent,
/// and a condition only reachable through a live Postgres is a condition nobody
/// tests.
///
/// True only when all three hold: a roster is actually configured (otherwise we
/// are already looking at the whole pool), the player's strength is known
/// (otherwise there is no bracket to satisfy), and the draw did not satisfy any
/// bracket step (including "no draw at all").
fn should_widen(use_roster: bool, human: Option<Skill>, draw: Option<BotDraw>) -> bool {
    if !use_roster || human.is_none() {
        return false;
    }
    match draw {
        None => true,
        Some(d) => d.step.is_none(),
    }
}

/// One bot-pool row as [`pick_bot_index`] sees it.
fn candidate_of_row(r: &CharacterDbEntryCharacterWalletInventory) -> BotCandidate {
    (r.id.to_string(), row_has_customization(r), skill_of_row(r))
}

/// Every OTHER character, capped. The pool used when no roster is configured, and
/// the widening the roster path falls back to when the bracket cannot be met.
async fn load_wide_pool(
    conn: &mut PooledConnection<'_, AsyncPgConnection>,
    human_char_uuid: &str,
) -> Vec<CharacterDbEntryCharacterWalletInventory> {
    let mut q = characters::table
        .select(CharacterDbEntryCharacterWalletInventory::as_select())
        .limit(200)
        .into_boxed();
    if let Ok(h) = Uuid::parse_str(human_char_uuid) {
        q = q.filter(characters::id.ne(h));
    }
    q.load(conn).await.unwrap_or_default()
}

#[cfg(test)]
mod human_priority_tests {
    use super::*;

    fn cfg() -> ArenaConfig {
        ArenaConfig {
            advertise_host: "127.0.0.1".into(),
            udp_port: 7777,
            max_concurrent_matches: 4,
            max_queued_players: 64,
            solo_fallback_secs: 4,
            debug_ghost_user_id: None,
            bot_user_ids: Vec::new(),
            busy_fallback_secs: 230,
            recent_fallback_secs: 30,
            recent_window_secs: 300,
        }
    }

    /// Tier ordering. Someone standing in the queue beats everything: the deadline is
    /// then just "stop honouring the bracket", not "wait for a maybe".
    #[test]
    fn the_tiers_are_ordered_waiting_then_live_then_recent_then_solo() {
        let c = cfg();
        let secs = |d: Duration| d.as_secs();
        assert_eq!(
            secs(fallback_delay(&c, 0, 0, 0)),
            c.solo_fallback_secs,
            "alone"
        );
        assert_eq!(
            secs(fallback_delay(&c, 0, 1, 0)),
            c.recent_fallback_secs,
            "seen lately"
        );
        assert_eq!(
            secs(fallback_delay(&c, 1, 1, 0)),
            c.busy_fallback_secs,
            "mid-match wins over recent"
        );
        assert_eq!(
            secs(fallback_delay(&c, 1, 1, 1)),
            c.solo_fallback_secs,
            "somebody queued RIGHT NOW outranks both — pair them, do not hold the door"
        );
    }

    /// The recent tier must clear a whole fight plus the results card, or two players
    /// trading fights drop out of each other's window mid-cycle.
    #[test]
    fn the_recent_window_outlasts_a_human_ai_match() {
        let c = cfg();
        // The five measured human-vs-AI matches from the 2026-08-03 prod trace in
        // `ArenaConfig::busy_fallback_secs`.
        let longest = *[81u64, 110, 78, 110, 84].iter().max().expect("non-empty");
        assert!(
            c.recent_window_secs > longest * 2,
            "recent window {}s must comfortably span a fight ({}s) plus a re-queue",
            c.recent_window_secs,
            longest
        );
        assert!(
            c.recent_fallback_secs > c.solo_fallback_secs,
            "the recent tier has to actually be longer than the solo one"
        );
    }

    /// 30 s is not arbitrary: it is the first deadline at which the whole bracket
    /// schedule is reachable. Below it the later widening steps are unreachable, which
    /// is the defect this PR is about — pinned so a tuning change has to see it.
    #[test]
    fn the_recent_tier_lets_the_bracket_fully_widen() {
        let c = cfg();
        let unlimited_at = (0..600)
            .find(|s| bracket_for(Duration::from_secs(*s)).is_none())
            .expect("the bracket gives up eventually");
        assert!(
            c.recent_fallback_secs >= unlimited_at,
            "recent tier {}s should reach the bracket's give-up point ({}s)",
            c.recent_fallback_secs,
            unlimited_at
        );
    }

    /// A player's own arrivals must not put them in the recent tier — that would mean a
    /// genuinely lone player waits 30 s for nobody.
    #[test]
    fn my_own_arrivals_do_not_count_as_company() {
        let now = Instant::now();
        let me = Uuid::new_v4();
        let log = vec![
            (me, now - Duration::from_secs(10)),
            (me, now - Duration::from_secs(200)),
        ];
        assert_eq!(
            others_recent(&log, me, Duration::from_secs(300), now),
            0,
            "queueing repeatedly alone must stay the solo tier"
        );
    }

    /// Distinct others are counted once; anyone outside the window is forgotten.
    #[test]
    fn others_recent_dedupes_and_expires() {
        let now = Instant::now();
        let me = Uuid::new_v4();
        let other = Uuid::new_v4();
        let ancient = Uuid::new_v4();
        let log = vec![
            (other, now - Duration::from_secs(30)),
            (other, now - Duration::from_secs(60)),
            (ancient, now - Duration::from_secs(3600)),
            (me, now),
        ];
        assert_eq!(
            others_recent(&log, me, Duration::from_secs(300), now),
            1,
            "one distinct other inside the window; the hour-old one is gone"
        );
    }

    /// THE CASE THE OWNER DESCRIBED: "if one is fighting an AI, let them finish before
    /// matching the other human to an AI, so they _will_ always match."
    ///
    /// B has been holding the queue open through A's fight (busy tier, 230 s). A's match
    /// ends, so `live_human_count()` drops and the tier falls to `recent`. Naively the
    /// deadline recomputes to `B.since + 30 s` — which, 100 s in, is 70 s IN THE PAST, so
    /// B is handed a bot at the exact moment A becomes free. B must instead get a fresh
    /// 30 s in which A can re-queue.
    #[test]
    fn a_deadline_never_jumps_into_the_past_when_the_arena_empties() {
        let now = Instant::now();
        let since = now - Duration::from_secs(100);
        let recent_delay = Duration::from_secs(30);

        let naive = since + recent_delay;
        assert!(
            naive < now,
            "precondition: the naive deadline is already behind us"
        );

        // Tier falls 2 (busy) -> 1 (recent).
        let floor = next_floor(
            since + Duration::from_secs(230),
            2,
            1,
            since,
            recent_delay,
            now,
        );
        assert!(
            floor > now,
            "the ticket must get a fresh grace window, not be botted the instant the \
             other player becomes available"
        );
        assert_eq!(
            floor,
            now + recent_delay,
            "the grace is the new tier's full delay, from now"
        );
    }

    /// A rising tier extends the deadline and never shortens it.
    #[test]
    fn a_rising_tier_only_ever_extends() {
        let now = Instant::now();
        let since = now - Duration::from_secs(3);
        let short = since + Duration::from_secs(4);
        let floor = next_floor(short, 0, 2, since, Duration::from_secs(230), now);
        assert_eq!(
            floor,
            since + Duration::from_secs(230),
            "solo -> busy extends"
        );
        assert!(floor > short);
    }

    /// A steady tier is stable — the floor must not creep forward every pass, or a
    /// ticket in a quiet arena would never fall back at all.
    #[test]
    fn a_steady_tier_does_not_creep() {
        let now = Instant::now();
        let since = now - Duration::from_secs(2);
        let delay = Duration::from_secs(4);
        let first = next_floor(since + delay, 0, 0, since, delay, now);
        let second = next_floor(first, 0, 0, since, delay, now + Duration::from_millis(500));
        assert_eq!(first, second, "same tier, same floor — no creep");
        assert_eq!(first, since + delay);
    }

    /// THE REPORTED BUG, second half. After the first human-vs-human match:
    /// "I changed to dwarven mail and frost, but it looked like I was fighting flappety
    /// in dragon bone... I think I may have fought you in my equipment too."
    ///
    /// An account can own several characters. `load_loadout` used to run
    /// `characters WHERE user_id = ?` and take `rows.into_iter().next()` — whatever
    /// Postgres returned first, with no ORDER BY. So the arena fought as an ARBITRARY
    /// character: wrong gear, wrong name, wrong stats, and the choice free to change
    /// between queries.
    ///
    /// The client tells us which character is queueing, in `matches/create`'s
    /// `playerId`. We bound the body to `_body` and threw it away.
    #[test]
    fn the_named_character_is_the_one_that_enters_the_arena() {
        let wanted = Uuid::new_v4();
        let rows = vec![
            character_row(Uuid::new_v4(), 900, 100), // strongest, and NOT the one playing
            character_row(wanted, 120, 41),          // the character actually queueing
        ];
        let picked = pick_character(rows, Some(wanted)).expect("a row is returned");
        assert_eq!(
            picked.id, wanted,
            "the arena must field the character the client named, not the best one on \
             the account"
        );
    }

    /// `playerId` arrives from the client, so it is not trusted on its own — the query
    /// is always scoped by user_id as well. Here the named id is simply not among this
    /// user's rows, which is what a forged or stale id looks like by the time it reaches
    /// the picker: fall back rather than return nothing.
    #[test]
    fn an_unknown_character_id_falls_back_instead_of_stranding_the_player() {
        let mine = Uuid::new_v4();
        let rows = vec![character_row(mine, 300, 55)];
        let picked = pick_character(rows, Some(Uuid::new_v4())).expect("falls back");
        assert_eq!(picked.id, mine, "an id this user does not own must not select it");
    }

    /// No character named: deterministic, and the SAME ordering `load_skill` uses. These
    /// used to disagree — skill took the max across all characters, the loadout took an
    /// arbitrary row — so a player could be bracketed as their strongest character and
    /// fight as another.
    #[test]
    fn the_fallback_is_the_strongest_character_not_an_arbitrary_row() {
        let best = Uuid::new_v4();
        let rows = vec![
            character_row(Uuid::new_v4(), 100, 90),
            character_row(best, 850, 60), // highest trophies wins, as load_skill does
            character_row(Uuid::new_v4(), 300, 99),
        ];
        assert_eq!(pick_character(rows, None).expect("a row").id, best);
    }

    /// Trophies first, level only as the tie-break — matching `load_skill`'s
    /// `max_by_key(|s| (s.trophies, s.level))` exactly.
    #[test]
    fn the_fallback_breaks_trophy_ties_by_level() {
        let higher_level = Uuid::new_v4();
        let rows = vec![
            character_row(Uuid::new_v4(), 400, 30),
            character_row(higher_level, 400, 80),
        ];
        assert_eq!(pick_character(rows, None).expect("a row").id, higher_level);
    }

    /// An account with no characters must not panic — matchmaking degrades to the
    /// starter loadout rather than refusing to queue.
    #[test]
    fn no_characters_is_none_not_a_panic() {
        assert!(pick_character(Vec::new(), Some(Uuid::new_v4())).is_none());
        assert!(pick_character(Vec::new(), None).is_none());
    }

    fn character_row(
        id: Uuid,
        trophies: i64,
        level: u16,
    ) -> crate::models::CharacterDbEntryCharacterWalletInventory {
        use crate::json_db::JsonDbWrapper;
        let mut c = blades_lib::user_data::CompleteCharacter::default();
        c.matchmaking_pvp_trophies = trophies;
        c.level = level;
        crate::models::CharacterDbEntryCharacterWalletInventory {
            id,
            user_id: Uuid::new_v4(),
            character: JsonDbWrapper(c),
            data: JsonDbWrapper(Default::default()),
            wallet: JsonDbWrapper(Default::default()),
            inventory: JsonDbWrapper(blades_lib::user_data::CompleteInventory {
                backpack: Default::default(),
                loadout: Default::default(),
                treasury: Default::default(),
                overflow_treasury: Default::default(),
                backpack_version: 1,
                treasury_version: 0,
            }),
        }
    }

    /// THE REPORTED BUG. Two testers press Fight seconds apart and never meet.
    ///
    /// The bracket and the solo fallback were designed independently and their
    /// constants disagree: the opening bracket (5 levels / 150 trophies) lasts 10 s,
    /// but a lone player falls back to a bot after 4 s. So a pair outside that opening
    /// bracket sits in `waiting` together and BOTH are handed a bot six seconds before
    /// the bracket would first have widened. The later bracket steps are unreachable in
    /// a two-player arena.
    ///
    /// Fails on the pre-fix code: the deadline branch resolved to a bot without ever
    /// looking at `waiting`.
    #[test]
    fn two_humans_outside_the_bracket_still_meet_at_last_call() {
        let now = Instant::now();
        // 18 levels and 400 trophies apart — refused by every bracket step until 20 s.
        let me = Some(Skill {
            level: 62,
            trophies: 610,
        });
        let adventurer = Some(Skill {
            level: 44,
            trophies: 210,
        });

        // They ARE mutually refused for the whole window before the bot fires.
        let solo = Duration::from_secs(cfg().solo_fallback_secs);
        assert!(
            !compatible(me, adventurer, solo),
            "precondition: the bracket still refuses this pair when the bot deadline fires"
        );

        // At last call the bracket is dropped and the human is taken.
        let waiting = [(adventurer, now - Duration::from_secs(2))];
        assert_eq!(
            last_call_partner(&waiting, me, now),
            Some(0),
            "a human out of bracket must beat a bot at the deadline"
        );
    }

    /// The constant relationship that caused the bug, pinned so a future edit to either
    /// side has to look at the other. If the solo fallback ever outlives the opening
    /// bracket step this assertion stops being interesting — and the last-call path
    /// stops being load-bearing.
    #[test]
    fn the_opening_bracket_outlives_the_solo_fallback() {
        let solo = cfg().solo_fallback_secs;
        let opening_step_ends = (0..)
            .find(|s| bracket_for(Duration::from_secs(*s)) != bracket_for(Duration::ZERO))
            .expect("the bracket widens at some point");
        assert!(
            solo < opening_step_ends,
            "solo fallback {solo}s vs first widening at {opening_step_ends}s — if the \
             fallback no longer fires inside the opening bracket, revisit last_call_partner"
        );
    }

    /// Last call still prefers the closest opponent — it drops the bracket, not the
    /// ordering. Otherwise a third player joining would make pairings arbitrary.
    #[test]
    fn last_call_takes_the_closest_human() {
        let now = Instant::now();
        let me = Some(Skill {
            level: 50,
            trophies: 400,
        });
        let far = Some(Skill {
            level: 90,
            trophies: 900,
        });
        let near = Some(Skill {
            level: 58,
            trophies: 480,
        });
        let waiting = [
            (far, now - Duration::from_secs(9)),
            (near, now - Duration::from_secs(1)),
        ];
        assert_eq!(
            last_call_partner(&waiting, me, now),
            Some(1),
            "closest in trophies wins even though the other waited longer"
        );
    }

    /// Equal gaps fall back to whoever has waited longest, so nobody is starved.
    #[test]
    fn last_call_breaks_ties_by_waiting_longest() {
        let now = Instant::now();
        let me = Some(Skill {
            level: 50,
            trophies: 400,
        });
        let a = Some(Skill {
            level: 55,
            trophies: 500,
        });
        let b = Some(Skill {
            level: 45,
            trophies: 300,
        });
        let waiting = [
            (a, now - Duration::from_secs(1)),
            (b, now - Duration::from_secs(8)),
        ];
        assert_eq!(
            last_call_partner(&waiting, me, now),
            Some(1),
            "same 100-trophy gap either way — the longer wait breaks it"
        );
    }

    /// An empty queue must still yield a bot. The whole point of the fallback is that a
    /// lone tester gets a fight; last call must not strand them.
    #[test]
    fn last_call_with_nobody_waiting_still_falls_back_to_a_bot() {
        let me = Some(Skill {
            level: 50,
            trophies: 400,
        });
        assert_eq!(
            last_call_partner(&[], me, Instant::now()),
            None,
            "nobody waiting -> no partner -> the bot path stays reachable"
        );
    }

    /// An unknown skill on either side must not exclude someone from last call — the
    /// same rule `compatible` follows. A failed lookup should cost pairing quality, not
    /// the match.
    #[test]
    fn last_call_accepts_an_unknown_skill() {
        let now = Instant::now();
        let waiting = [(None, now - Duration::from_secs(3))];
        assert_eq!(
            last_call_partner(
                &waiting,
                Some(Skill {
                    level: 50,
                    trophies: 400
                }),
                now
            ),
            Some(0),
            "unknown skill still beats a bot"
        );
        assert_eq!(
            last_call_partner(
                &[(
                    Some(Skill {
                        level: 1,
                        trophies: 0
                    }),
                    now
                )],
                None,
                now
            ),
            Some(0),
            "and the lone player's own unknown skill must not strand them either"
        );
    }

    /// Alone in the arena → a bot straight away. This is the case the 4 s fallback
    /// was written for and it must not regress: one tester must never sit in
    /// "Searching".
    #[test]
    fn a_lone_player_still_gets_a_bot_fast() {
        assert_eq!(fallback_delay(&cfg(), 0, 0, 0), Duration::from_secs(4));
    }

    /// Somebody else is mid-match → hold the queue open for them.
    #[test]
    fn a_busy_arena_makes_the_bot_wait() {
        assert_eq!(fallback_delay(&cfg(), 1, 0, 0), Duration::from_secs(230));
        assert_eq!(fallback_delay(&cfg(), 7, 0, 0), Duration::from_secs(230));
    }

    /// **The reason for the number.** Prod 2026-08-03: two players shared the arena
    /// for six minutes and never met, because their cycles were offset by ~50 s and
    /// the fallback was 4 s. Measured human-vs-AI matches were 81, 110, 78, 110 and
    /// 84 seconds — mean 92.6, longest 110.
    ///
    /// For the second player to arrive while the first is still queued, the wait has
    /// to outlast a whole match plus the menu time before a re-queue. One match is
    /// not enough; this asserts the default clears the LONGEST observed match with
    /// room to spare, which is what "100-200 % longer" buys.
    #[test]
    fn the_busy_wait_outlasts_a_whole_human_ai_match() {
        const OBSERVED_MATCH_SECS: &[u64] = &[81, 110, 78, 110, 84];
        let mean = OBSERVED_MATCH_SECS.iter().sum::<u64>() / OBSERVED_MATCH_SECS.len() as u64;
        let longest = *OBSERVED_MATCH_SECS.iter().max().unwrap();
        let busy = cfg().busy_fallback_secs;

        assert!(
            busy >= mean * 2,
            "the busy wait ({busy}s) must be at least 100% longer than the mean \
             human-vs-AI match ({mean}s), or two offset players still miss each other"
        );
        assert!(
            busy <= mean * 3,
            "…and at most 200% longer ({}s), or a player whose opponent quietly left \
             waits absurdly long before the deadline collapses",
            mean * 3
        );
        assert!(
            busy > longest,
            "it must clear the LONGEST observed match ({longest}s), not just the mean"
        );
    }

    /// **The wiring.** Everything above passes even if the loop never asks the
    /// registry how many people are playing — so this drives the seam that actually
    /// reads it, with a real registry and a real admitted peer.
    #[test]
    fn the_deadline_reflects_who_is_actually_playing() {
        use crate::arena::combat::Loadout;
        let reg = MatchRegistry::new(4);
        let config = cfg();
        let since = Instant::now();

        // Empty arena → the fast deadline.
        assert_eq!(
            fallback_deadline(&config, &reg, since, 0, 0, None, Instant::now()).0,
            since + Duration::from_secs(4),
            "nobody else is playing, so a lone player must not be made to wait"
        );

        // One human mid-match → the long deadline.
        let psid = "aaaaaaaa-0000-0000-0000-000000000000";
        assert!(reg.allocate_with_bots(
            &[psid.to_string()],
            vec![Loadout::default(), Loadout::default()],
            Uuid::new_v4(),
            1,
        ));
        let peer: std::net::SocketAddr = "10.0.0.1:5000".parse().unwrap();
        assert!(reg.admit(peer, psid, &[7u8; 32]).is_some());
        assert_eq!(
            fallback_deadline(&config, &reg, since, 0, 0, None, Instant::now()).0,
            since + Duration::from_secs(230),
            "somebody is mid-match — hold the queue open for them"
        );

        // They leave → back to the fast deadline, so a departed player cannot strand
        // whoever is waiting.
        reg.remove(&peer);
        assert_eq!(
            fallback_deadline(&config, &reg, since, 0, 0, None, Instant::now()).0,
            since + Duration::from_secs(4),
            "the delay must collapse when the arena empties"
        );
    }

    /// The delay must be reachable from the environment without a code change — the
    /// right number is a matter of how many people are actually playing, and that
    /// changes without us.
    #[test]
    fn the_busy_wait_is_configurable_and_defaults_sanely() {
        // Defaults come from `from_env`; assert the default here rather than mutating
        // process env (which races other tests in the same binary).
        assert_eq!(cfg().busy_fallback_secs, 230);
        assert!(cfg().busy_fallback_secs > cfg().solo_fallback_secs);
    }
}

#[cfg(test)]
mod bot_pick_tests {
    use super::*;

    /// A bot opponent must be identifiable as one, in BOTH places the client
    /// reads a name. Requested 2026-08-03: a solo match loads a real character
    /// from the bot roster, so without this the opponent wears a real player's
    /// name and there is no way to tell a script from a person.
    #[test]
    fn bot_loadout_is_marked_ai_in_both_name_fields() {
        let mut lo = crate::arena::combat::loadout::starter();
        lo.display_name = "Blank".into();
        lo.profile_character_json = r#"{"id":"abc","name":"Blank","tagId":7}"#.to_string();

        mark_loadout_as_bot(&mut lo);

        assert_eq!(lo.display_name, "Blank (AI)", "the op50 Player spawn name");
        let v: serde_json::Value =
            serde_json::from_str(&lo.profile_character_json).expect("profile stays valid JSON");
        assert_eq!(
            v["name"], "Blank (AI)",
            "the op54 PROFILE name the HUD renders"
        );
        // Everything else in the profile survives untouched — this is a rename,
        // not a rebuild.
        assert_eq!(v["id"], "abc");
        assert_eq!(v["tagId"], 7);
    }

    /// **The wiring, not just the helper.** The unit tests above pass even if
    /// nothing ever CALLS `mark_loadout_as_bot` — which is exactly how a fix ships
    /// green and does nothing. This drives the real entry point.
    ///
    /// The no-database path is the one testable without a pool, and it was also
    /// one of the two early returns the first version of this change forgot: a bot
    /// loaded with no DB came back wearing an unmarked name.
    #[test]
    fn load_bot_loadout_marks_even_the_no_database_path() {
        let config = ArenaConfig {
            advertise_host: "127.0.0.1".into(),
            udp_port: 7777,
            max_concurrent_matches: 4,
            max_queued_players: 64,
            solo_fallback_secs: 15,
            debug_ghost_user_id: None,
            bot_user_ids: Vec::new(),
            busy_fallback_secs: 230,
            recent_fallback_secs: 30,
            recent_window_secs: 300,
        };
        // No DB pool → the function's first early return. `block_on` because the
        // path never awaits anything real once `db` is None.
        let lo = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(load_bot_loadout(
                &None,
                "00000000-0000-0000-0000-000000000000",
                None,
                &config,
                Uuid::nil(),
            ));
        assert!(
            lo.display_name.ends_with(" (AI)"),
            "a bot loaded without a database must still be marked, got {:?}",
            lo.display_name
        );
    }

    /// `load_bot_loadout` has several return paths and could grow another. Marking
    /// twice must not produce "Blank (AI) (AI)".
    #[test]
    fn marking_a_bot_twice_does_not_stack_the_suffix() {
        let mut lo = crate::arena::combat::loadout::starter();
        lo.display_name = "Blank".into();
        lo.profile_character_json = r#"{"name":"Blank"}"#.to_string();
        mark_loadout_as_bot(&mut lo);
        mark_loadout_as_bot(&mut lo);
        assert_eq!(lo.display_name, "Blank (AI)");
        let v: serde_json::Value = serde_json::from_str(&lo.profile_character_json).unwrap();
        assert_eq!(v["name"], "Blank (AI)");
    }

    /// The `starter()` fallback has NO name, and the engine substitutes "Fighter"
    /// for an empty one. Appending to "" would make it non-empty and put a bare
    /// " (AI)" on screen instead.
    #[test]
    fn a_nameless_bot_becomes_fighter_ai_not_just_ai() {
        let mut lo = crate::arena::combat::loadout::starter();
        assert_eq!(lo.display_name, "", "precondition: starter has no name");
        mark_loadout_as_bot(&mut lo);
        assert_eq!(lo.display_name, "Fighter (AI)");
    }

    /// A malformed or empty profile must not be replaced by a differently broken
    /// one — an unnamed opponent still has to be able to fight.
    #[test]
    fn a_profile_that_is_not_json_is_left_alone() {
        let mut lo = crate::arena::combat::loadout::starter();
        lo.display_name = "Blank".into();
        lo.profile_character_json = "not json at all".to_string();
        mark_loadout_as_bot(&mut lo);
        assert_eq!(lo.display_name, "Blank (AI)", "the name still gets marked");
        assert_eq!(
            lo.profile_character_json, "not json at all",
            "a broken profile is left exactly as it was"
        );
    }

    fn gsid_with_first_byte(b: u8) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[0] = b;
        Uuid::from_bytes(bytes)
    }

    /// A candidate whose strength is unknown — the shape these tests used before the
    /// bot bracket existed, so they keep asserting exactly what they asserted then.
    fn cand(uuid: &str, complete: bool) -> BotCandidate {
        (uuid.to_string(), complete, None)
    }

    /// A candidate at a known level / trophy count.
    fn cand_at(uuid: &str, level: i32, trophies: i64) -> BotCandidate {
        (uuid.to_string(), true, Some(Skill { level, trophies }))
    }

    /// These tests assert WHICH candidate is drawn. The bracket step that justified
    /// the draw is asserted separately, by the tests below that care about it, so
    /// the index-only assertions stay readable.
    fn pick_bot_index(
        candidates: &[BotCandidate],
        human_char_uuid: &str,
        human: Option<Skill>,
        gsid: Uuid,
    ) -> Option<usize> {
        super::pick_bot_index(candidates, human_char_uuid, human, gsid).map(|d| d.index)
    }

    /// The bracket step a draw satisfied, or `None` for a fallback draw.
    fn drawn_step(
        candidates: &[BotCandidate],
        human: Option<Skill>,
        seed: u8,
    ) -> Option<(i32, i64)> {
        super::pick_bot_index(candidates, NOBODY, human, gsid_with_first_byte(seed))
            .expect("a candidate was available")
            .step
    }

    // -------------------------------------------------------------------
    // tracker #49: the roster is a preference, not a cage
    // -------------------------------------------------------------------

    /// The bot roster exactly as production had it on 2026-08-21, minus the
    /// reporter's own character (self-matches are filtered before the bracket).
    fn production_roster() -> Vec<BotCandidate> {
        vec![
            cand_at("11111111-0000-0000-0000-000000000001", 16, 145), // Prki
            cand_at("22222222-0000-0000-0000-000000000002", 68, 790), // Meryl Andra
            cand_at("33333333-0000-0000-0000-000000000003", 89, 776), // WolfWalker
            cand_at("44444444-0000-0000-0000-000000000004", 91, 1568), // Ivan
            cand_at("55555555-0000-0000-0000-000000000005", 93, 777), // Shoyr
        ]
    }

    /// N'wah, the reporting player: level 43, 49 trophies.
    fn nwah() -> Option<Skill> {
        Some(Skill {
            level: 43,
            trophies: 49,
        })
    }

    /// THE reported match. Against the real roster nobody is inside even the widest
    /// step — Prki is 27 levels away, everyone else is 25+ levels AND 700+ trophies
    /// away — so the draw is a fallback and must SAY so. Before this it was silent,
    /// which is why the mismatch went unexplained.
    #[test]
    fn the_production_roster_cannot_bracket_a_level_43_player() {
        for seed in [0u8, 1, 2, 3, 4, 200] {
            assert_eq!(
                drawn_step(&production_roster(), nwah(), seed),
                None,
                "seed {seed}: no roster member is within {:?}",
                BRACKET_STEPS[BRACKET_STEPS.len() - 1],
            );
        }
    }

    /// …and the fix: three characters OUTSIDE the roster (also production, same
    /// day) sit inside the TIGHTEST step. Widening finds them, so the mismatch was
    /// never necessary.
    #[test]
    fn the_full_pool_brackets_the_same_player_at_the_tightest_step() {
        let mut wide = production_roster();
        wide.extend([
            cand_at("66666666-0000-0000-0000-000000000006", 38, 56), // Ruukoto
            cand_at("77777777-0000-0000-0000-000000000007", 40, 157), // Ma'dami
            cand_at("88888888-0000-0000-0000-000000000008", 47, 70), // Ulvoch
        ]);
        for seed in [0u8, 1, 2, 3, 4, 200] {
            assert_eq!(
                drawn_step(&wide, nwah(), seed),
                Some(BRACKET_STEPS[0]),
                "seed {seed}: the near-level characters must win over the roster",
            );
            let i = super::pick_bot_index(&wide, NOBODY, nwah(), gsid_with_first_byte(seed))
                .unwrap()
                .index;
            assert!(
                i >= 5,
                "seed {seed}: drew index {i}, expected one of the three near ones"
            );
        }
    }

    /// A draw that DID satisfy a bracket reports which one — otherwise the caller
    /// cannot tell a good draw from a tolerated mismatch, and would widen (or warn)
    /// on every match.
    #[test]
    fn a_bracketed_draw_reports_its_step() {
        let near = vec![cand_at("99999999-0000-0000-0000-000000000009", 45, 100)];
        assert_eq!(drawn_step(&near, nwah(), 0), Some(BRACKET_STEPS[0]));

        // 20 levels / 500 trophies apart: too far for steps 0 and 1, inside step 2.
        let middling = vec![cand_at("99999999-0000-0000-0000-00000000000a", 63, 549)];
        assert_eq!(drawn_step(&middling, nwah(), 0), Some(BRACKET_STEPS[2]));
    }

    /// Widening must never smuggle in an opponent that cannot render — that is the
    /// invisible-bot bug the roster was introduced to prevent. A perfectly
    /// bracketed but INCOMPLETE candidate stays excluded, and the draw stays a
    /// fallback rather than silently becoming "bracketed".
    #[test]
    fn widening_does_not_relax_the_completeness_filter() {
        let mut wide = production_roster();
        wide.push((
            "aaaaaaaa-0000-0000-0000-00000000000b".to_string(),
            false, // incomplete: an invisible opponent
            Some(Skill {
                level: 43,
                trophies: 49,
            }), // a perfect bracket match
        ));
        assert_eq!(
            drawn_step(&wide, nwah(), 0),
            None,
            "an unrenderable character must not be drawn, however well it brackets",
        );
    }

    #[test]
    fn widen_only_when_the_roster_failed_the_bracket() {
        let bracketed = Some(BotDraw {
            index: 0,
            step: Some(BRACKET_STEPS[0]),
        });
        let fallback = Some(BotDraw {
            index: 0,
            step: None,
        });

        assert!(
            should_widen(true, nwah(), fallback),
            "roster missed the bracket, so widen"
        );
        assert!(
            should_widen(true, nwah(), None),
            "roster had nobody at all, so widen"
        );
        assert!(
            !should_widen(true, nwah(), bracketed),
            "a bracketed roster draw is what we wanted, so no second query"
        );
        assert!(
            !should_widen(false, nwah(), fallback),
            "no roster configured, so the draw already came from the whole pool"
        );
        assert!(
            !should_widen(true, None, fallback),
            "unknown player strength, so there is no bracket to satisfy"
        );
    }

    #[test]
    fn pick_bot_index_prefers_complete_and_distinct() {
        let human = "aaaaaaaa-0000-0000-0000-000000000001";
        let cands = vec![
            cand(human, true),                                   // self-match → excluded
            cand("bbbbbbbb-0000-0000-0000-000000000002", false), // incomplete → excluded
            cand("cccccccc-0000-0000-0000-000000000003", true),  // the only eligible
        ];
        assert_eq!(
            pick_bot_index(&cands, human, None, gsid_with_first_byte(0)),
            Some(2)
        );
        assert_eq!(
            pick_bot_index(&cands, human, None, gsid_with_first_byte(123)),
            Some(2)
        );
    }

    #[test]
    fn pick_bot_index_none_when_no_complete_distinct() {
        let human = "aaaaaaaa-0000-0000-0000-000000000001";
        let cands = vec![
            cand(human, true),                                   // self
            cand("dddddddd-0000-0000-0000-000000000004", false), // incomplete
        ];
        assert_eq!(
            pick_bot_index(&cands, human, None, gsid_with_first_byte(0)),
            None
        );
    }

    // -------------------------------------------------------------------
    // tracker #24: the bot draw goes through the human bracket
    // -------------------------------------------------------------------

    const NOBODY: &str = "zzzzzzzz-0000-0000-0000-000000000009";

    /// THE reported case. Level 43, and the pool holds the four bot levels he
    /// actually drew (89, 93, 66, 89) plus one bot near him. The near one must win
    /// regardless of the gsid rotation — before this, every seed was equally likely
    /// to hand him the level 93.
    #[test]
    fn a_level_43_player_draws_the_bot_near_his_level() {
        let taheen = Some(Skill {
            level: 43,
            trophies: 240,
        });
        let cands = vec![
            cand_at("11111111-0000-0000-0000-000000000001", 89, 700),
            cand_at("22222222-0000-0000-0000-000000000002", 93, 810),
            cand_at("33333333-0000-0000-0000-000000000003", 66, 520),
            cand_at("44444444-0000-0000-0000-000000000004", 89, 690),
            cand_at("55555555-0000-0000-0000-000000000005", 45, 300), // the fair one
        ];
        for seed in 0u8..=255 {
            assert_eq!(
                pick_bot_index(&cands, NOBODY, taheen, gsid_with_first_byte(seed)),
                Some(4),
                "seed {seed} must still draw the level-45 bot, not one of the 66-93s",
            );
        }
        // And the old behaviour really did hand him the far ones: with no skill on
        // either side the rotation walks the whole pool.
        let blind: Vec<Option<usize>> = (0u8..5)
            .map(|s| pick_bot_index(&cands, NOBODY, None, gsid_with_first_byte(s)))
            .collect();
        assert_eq!(blind, vec![Some(0), Some(1), Some(2), Some(3), Some(4)]);
    }

    /// The ladder is walked TIGHTEST FIRST, and it is the human bracket's own table.
    /// A bot inside step 0 beats a bot that only makes step 1, which beats step 2.
    #[test]
    fn the_bot_ladder_takes_the_tightest_step_that_has_anyone() {
        let me = Some(Skill {
            level: 50,
            trophies: 400,
        });
        // One candidate per step, plus one outside every step.
        let cands = vec![
            cand_at("00000000-0000-0000-0000-00000000000f", 90, 2000), // outside all
            cand_at("00000000-0000-0000-0000-000000000003", 68, 970), // outside all (level 18 ok, trophies 570 → step 2)
            cand_at("00000000-0000-0000-0000-000000000002", 59, 690), // step 1 (9 / 290)
            cand_at("00000000-0000-0000-0000-000000000001", 53, 500), // step 0 (3 / 100)
        ];
        for seed in [0u8, 1, 7, 128, 255] {
            assert_eq!(
                pick_bot_index(&cands, NOBODY, me, gsid_with_first_byte(seed)),
                Some(3),
                "the step-0 candidate wins outright",
            );
        }
        // Drop it → the step-1 candidate. Then the step-2 one. Then anyone.
        let mut pool = cands.clone();
        pool.remove(3);
        assert_eq!(
            pick_bot_index(&pool, NOBODY, me, gsid_with_first_byte(0)),
            Some(2)
        );
        pool.remove(2);
        assert_eq!(
            pick_bot_index(&pool, NOBODY, me, gsid_with_first_byte(0)),
            Some(1)
        );
        pool.remove(1);
        // Only the far one is left: a bad match beats no match.
        assert_eq!(
            pick_bot_index(&pool, NOBODY, me, gsid_with_first_byte(0)),
            Some(0)
        );
    }

    /// The bracket is a PREFERENCE, never a gate. An unusable pool must still yield
    /// an opponent rather than strand the player at "determining server".
    #[test]
    fn the_bot_bracket_never_refuses_to_start_a_match() {
        let me = Some(Skill {
            level: 5,
            trophies: 10,
        });
        let far = vec![cand_at("00000000-0000-0000-0000-000000000001", 100, 5000)];
        assert_eq!(
            pick_bot_index(&far, NOBODY, me, gsid_with_first_byte(0)),
            Some(0),
            "nobody in any step → still pick the far bot",
        );
        // Unknown human skill (a failed `load_skill`) → the pre-#24 rotation.
        assert_eq!(
            pick_bot_index(&far, NOBODY, None, gsid_with_first_byte(0)),
            Some(0)
        );
        // Unknown CANDIDATE skill → it cannot satisfy a step, but it can still be drawn.
        let unknown = vec![cand("00000000-0000-0000-0000-000000000002", true)];
        assert_eq!(
            pick_bot_index(&unknown, NOBODY, me, gsid_with_first_byte(0)),
            Some(0)
        );
        // And an INCOMPLETE candidate is still excluded — the render guard outranks
        // the bracket, because an invisible opponent hangs the client at "Connecting".
        let incomplete = vec![(
            "00000000-0000-0000-0000-000000000003".to_string(),
            false,
            Some(Skill {
                level: 5,
                trophies: 10,
            }),
        )];
        assert_eq!(
            pick_bot_index(&incomplete, NOBODY, me, gsid_with_first_byte(0)),
            None
        );
    }

    /// Variety across matches must survive inside a tier: two bots equally close to
    /// the player still rotate by gsid, as they did before the bracket existed.
    #[test]
    fn the_gsid_rotation_still_varies_within_a_tier() {
        let me = Some(Skill {
            level: 50,
            trophies: 400,
        });
        let cands = vec![
            cand_at("00000000-0000-0000-0000-000000000001", 48, 350),
            cand_at("00000000-0000-0000-0000-000000000002", 52, 450),
            cand_at("00000000-0000-0000-0000-000000000003", 95, 3000), // outside every step
        ];
        assert_eq!(
            pick_bot_index(&cands, NOBODY, me, gsid_with_first_byte(0)),
            Some(0)
        );
        assert_eq!(
            pick_bot_index(&cands, NOBODY, me, gsid_with_first_byte(1)),
            Some(1)
        );
        assert_eq!(
            pick_bot_index(&cands, NOBODY, me, gsid_with_first_byte(2)),
            Some(0),
            "the rotation wraps within the tier and never reaches the level-95 bot",
        );
    }

    /// A self-match is excluded before the bracket is consulted — it is the
    /// "opponent actor never instantiates" hang, not a fairness question.
    #[test]
    fn a_perfectly_matched_self_is_still_refused() {
        let human = "aaaaaaaa-0000-0000-0000-000000000001";
        let me = Some(Skill {
            level: 50,
            trophies: 400,
        });
        let cands = vec![
            (
                human.to_string(),
                true,
                Some(Skill {
                    level: 50,
                    trophies: 400,
                }),
            ),
            cand_at("00000000-0000-0000-0000-000000000002", 95, 3000),
        ];
        assert_eq!(
            pick_bot_index(&cands, human, me, gsid_with_first_byte(0)),
            Some(1),
            "the far bot beats fighting yourself",
        );
    }

    /// The human and bot paths must share one table. If `bracket_for` and
    /// `BRACKET_STEPS` ever diverge, a bot could be a worse match than a human
    /// would have been allowed to be — the exact defect this fixes.
    #[test]
    fn the_human_bracket_and_the_bot_ladder_are_the_same_table() {
        for (i, secs) in [0u64, 10, 20].iter().enumerate() {
            assert_eq!(
                bracket_for(Duration::from_secs(*secs)),
                Some(BRACKET_STEPS[i]),
                "bracket step {i} must be BRACKET_STEPS[{i}]",
            );
        }
        // Tightest first, and monotonically widening.
        for w in BRACKET_STEPS.windows(2) {
            assert!(
                w[1].0 >= w[0].0 && w[1].1 >= w[0].1,
                "the ladder must not narrow"
            );
        }
    }

    /// The pairing that produced tracker #19: Taheen at level 43 against a level
    /// 66. It must be refused on arrival and allowed only once the queue has
    /// widened — a server with a handful of players cannot hold out forever.
    #[test]
    fn the_reported_mismatch_is_refused_at_first_and_allowed_later() {
        let taheen = Some(Skill {
            level: 43,
            trophies: 240,
        });
        let trickster = Some(Skill {
            level: 66,
            trophies: 720,
        });

        assert!(
            !compatible(taheen, trickster, Duration::from_secs(0)),
            "43 vs 66 must not pair immediately — this is the reported bug"
        );
        assert!(!compatible(taheen, trickster, Duration::from_secs(15)));
        assert!(!compatible(taheen, trickster, Duration::from_secs(25)));
        assert!(
            compatible(taheen, trickster, Duration::from_secs(30)),
            "after 30s a match beats no match"
        );
    }

    #[test]
    fn a_close_pairing_goes_through_at_once() {
        let a = Some(Skill {
            level: 45,
            trophies: 300,
        });
        let b = Some(Skill {
            level: 47,
            trophies: 380,
        });
        assert!(compatible(a, b, Duration::from_secs(0)));
    }

    /// Level and trophies are BOTH gates, not either/or. A player who is close on
    /// one and far on the other is not a fair match.
    #[test]
    fn both_dimensions_are_enforced() {
        let base = Some(Skill {
            level: 50,
            trophies: 400,
        });
        let same_level_far_trophies = Some(Skill {
            level: 51,
            trophies: 900,
        });
        let same_trophies_far_level = Some(Skill {
            level: 80,
            trophies: 410,
        });
        assert!(!compatible(
            base,
            same_level_far_trophies,
            Duration::from_secs(0)
        ));
        assert!(!compatible(
            base,
            same_trophies_far_level,
            Duration::from_secs(0)
        ));
    }

    /// A failed skill lookup must never strand someone in the queue. The bracket is
    /// a preference, not a gate that can deadlock the arena.
    #[test]
    fn unknown_skill_never_blocks_a_match() {
        let known = Some(Skill {
            level: 1,
            trophies: 0,
        });
        let wildly_different = Some(Skill {
            level: 100,
            trophies: 5000,
        });
        assert!(compatible(None, wildly_different, Duration::from_secs(0)));
        assert!(compatible(known, None, Duration::from_secs(0)));
        assert!(compatible(None, None, Duration::from_secs(0)));
    }

    /// The bracket must actually widen, or the first step is the only step.
    #[test]
    fn the_bracket_widens_then_gives_up() {
        let steps: Vec<Option<(i32, i64)>> = [0u64, 10, 20, 30]
            .iter()
            .map(|s| bracket_for(Duration::from_secs(*s)))
            .collect();
        assert_eq!(steps[0], Some((5, 150)));
        assert_eq!(steps[1], Some((10, 300)));
        assert_eq!(steps[2], Some((20, 600)));
        assert_eq!(steps[3], None, "eventually anyone will do");
        // Monotonic: a longer wait is never stricter.
        for w in [(0u64, 10u64), (10, 20)] {
            let (a, b) = (
                bracket_for(Duration::from_secs(w.0)).unwrap(),
                bracket_for(Duration::from_secs(w.1)).unwrap(),
            );
            assert!(
                b.0 >= a.0 && b.1 >= a.1,
                "bracket must not narrow with time"
            );
        }
    }

    #[test]
    fn pick_bot_index_rotates_across_matches() {
        let human = "zzzzzzzz-0000-0000-0000-000000000009";
        let cands = vec![
            cand("11111111-0000-0000-0000-000000000001", true),
            cand("22222222-0000-0000-0000-000000000002", true),
            cand("33333333-0000-0000-0000-000000000003", true),
        ];
        // Three eligible → gsid first-byte selects eligible[b % 3]; distinct seeds differ.
        assert_eq!(
            pick_bot_index(&cands, human, None, gsid_with_first_byte(0)),
            Some(0)
        );
        assert_eq!(
            pick_bot_index(&cands, human, None, gsid_with_first_byte(1)),
            Some(1)
        );
        assert_eq!(
            pick_bot_index(&cands, human, None, gsid_with_first_byte(2)),
            Some(2)
        );
    }
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

/// The symmetric half of [`check_paired_uuids_distinct`], for the op54 PROFILE.
///
/// `character_uuid` is the KEY the client binds appearance by; `profile_character_json`
/// is the VALUE it dresses the avatar from. A distinct key with a missing or shared
/// value collapses identity just as thoroughly, and it fails in a way that is easy to
/// ship by accident, because [`loadout::starter`] — the fallback whenever a character
/// load is slow, errors, or the row is missing — has an EMPTY profile:
///   - **empty** → `broadcast_profiles` skips that fighter, so the opponent never gets
///     an op54 profile at all and the client leaves the opponent body wearing whatever
///     it already has (the local character's customization);
///   - **identical** → both clients dress both avatars from the same blob.
///
/// `Ok(())` when every fighter has a non-empty, distinct `profile_character_json`.
/// Kept separate from the UUID guard so each failure names its own cause (and so the
/// UUID guard's own unit test can keep using bare `starter()` loadouts).
fn check_paired_profiles_present_and_distinct(
    loadouts: &[crate::arena::combat::Loadout],
) -> Result<(), String> {
    for (i, lo) in loadouts.iter().enumerate() {
        if lo.profile_character_json.is_empty() {
            return Err(format!(
                "fighter {i} (\"{}\", char {}) has an EMPTY profile_character_json — a degraded \
                 loadout::starter() fallback. broadcast_profiles skips empty profiles, so the \
                 opponent never receives this fighter's op54 PROFILE and renders its body with \
                 the LOCAL character's appearance.",
                lo.display_name, lo.character_uuid,
            ));
        }
        for (j, other) in loadouts.iter().enumerate().skip(i + 1) {
            if lo.profile_character_json == other.profile_character_json {
                return Err(format!(
                    "fighters {i} (\"{}\") and {j} (\"{}\") share an IDENTICAL \
                     profile_character_json ({} B) — both clients would dress both avatars from \
                     the same customization blob.",
                    lo.display_name,
                    other.display_name,
                    lo.profile_character_json.len(),
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
    let Ok(mut conn) = db.get().await else {
        return Vec::new();
    };
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
    pub matchmaker_tx: UnboundedSender<MatchmakerCommand>,
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
        let registry =
            MatchRegistry::new_with_submitter(config.max_concurrent_matches, key_submitter);

        let (tx, rx) = unbounded_channel::<MatchmakerCommand>();
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

/// How often the fallback branch re-checks whether anyone else is playing, while a
/// ticket waits. Short enough that the deadline collapses promptly when the last other
/// player leaves; long enough to be free (it only ticks while someone is queued).
const FALLBACK_REEVALUATE: Duration = Duration::from_secs(2);

/// How long a waiting ticket may hold out for a human, given how many other humans
/// are in a live match right now.
///
/// Pulled out of the loop so the rule is testable without spinning a matchmaker, a
/// registry and a clock — the loop then has no decision left to get wrong.
fn fallback_delay(
    config: &ArenaConfig,
    others_live: usize,
    others_recent: usize,
    others_waiting: usize,
) -> Duration {
    if others_waiting > 0 {
        // Somebody else is standing in the queue RIGHT NOW. Nothing is gained by
        // holding longer — the only thing keeping these two apart is the bracket, and
        // the deadline is precisely where `last_call_partner` stops honouring it.
        // Waiting the `recent` tier here would make two coordinated players stare at
        // "determining server" for 30 s to reach a pairing available at 4.
        Duration::from_secs(config.solo_fallback_secs)
    } else if others_live > 0 {
        // Somebody is mid-match and about to be free.
        Duration::from_secs(config.busy_fallback_secs)
    } else if others_recent > 0 {
        // Nobody is playing right now, but somebody else queued inside the recent
        // window — they are between fights, not gone. Hold the queue open long enough
        // that a Discord-coordinated pair cannot miss each other.
        Duration::from_secs(config.recent_fallback_secs)
    } else {
        // Genuinely alone. Never make this player wait.
        Duration::from_secs(config.solo_fallback_secs)
    }
}

/// Distinct OTHER humans seen queuing inside `recent_window_secs`.
///
/// "Other" is load-bearing: the caller's own arrivals are in the log too, and counting
/// them would put a lone player permanently in the recent tier — a 30 s stare at
/// "determining server" for someone with nobody to match against.
fn others_recent(log: &[(Uuid, Instant)], me: Uuid, window: Duration, now: Instant) -> usize {
    let mut seen: Vec<Uuid> = Vec::new();
    for (user, at) in log {
        if *user != me && now.saturating_duration_since(*at) <= window && !seen.contains(user) {
            seen.push(*user);
        }
    }
    seen.len()
}

fn tier_rank(others_live: usize, others_recent: usize, others_waiting: usize) -> u8 {
    if others_waiting > 0 {
        3
    } else if others_live > 0 {
        2
    } else if others_recent > 0 {
        1
    } else {
        0
    }
}

/// The earliest a ticket may fall back, given what just changed.
///
/// **A deadline must never jump into the past.** Without this, the moment the player we
/// were holding the queue open for finishes their match, `live_human_count()` drops and
/// the deadline recomputes to `since + <shorter delay>` — which, for anyone who has
/// already waited longer than that, is a time that has passed. They are handed a bot in
/// the same instant the human became available. That is the reverse of the intent: the
/// point of waiting through someone else's fight is to be there when it ends.
///
/// So when the tier FALLS, the shorter delay is granted **from now** — a fresh grace
/// window in which the other player can re-queue. When it rises we extend. The grace is
/// self-limiting: once the other player drops out of `recent_window_secs` the tier falls
/// to solo and the next grace is only `solo_fallback_secs`.
fn next_floor(
    current: Instant,
    previous_tier: u8,
    tier: u8,
    since: Instant,
    delay: Duration,
    now: Instant,
) -> Instant {
    if tier < previous_tier {
        now + delay
    } else {
        current.max(since + delay)
    }
}

/// Tier name for the log line that explains a deadline. The difference between "why did
/// I get a bot" being one grep or an afternoon.
fn fallback_tier(others_live: usize, others_recent: usize, others_waiting: usize) -> &'static str {
    if others_waiting > 0 {
        "paired-at-deadline (another human is queued)"
    } else if others_live > 0 {
        "busy (someone is mid-match)"
    } else if others_recent > 0 {
        "recent (someone queued lately)"
    } else {
        "solo (nobody else around)"
    }
}

/// When a ticket that started waiting at `since` may fall back to a bot.
///
/// The seam that READS the arena's state. Split from [`fallback_delay`] so a test can
/// pin the wiring — a test of `fallback_delay` alone passes even if the loop never
/// asks the registry anything, which is how this feature would ship green and do
/// nothing at all.
fn fallback_deadline(
    config: &ArenaConfig,
    registry: &MatchRegistry,
    since: Instant,
    recent: usize,
    waiting_others: usize,
    prev: Option<(Instant, u8)>,
    now: Instant,
) -> (Instant, u8) {
    let live = registry.live_human_count();
    let delay = fallback_delay(config, live, recent, waiting_others);
    let tier = tier_rank(live, recent, waiting_others);
    let floor = match prev {
        None => since + delay,
        Some((current, previous_tier)) => next_floor(current, previous_tier, tier, since, delay, now),
    };
    (floor, tier)
}

/// The matchmaker actor. Single owner of the ticket queue — no locks.
///
/// A lone ticket waits for a human opponent to PAIR with; if none arrives before its
/// deadline it falls back to a solo match against a server-driven bot, so a single
/// tester always gets a fight instead of being stuck "Searching".
///
/// **HUMANS GET FIRST REFUSAL.** The deadline is not one number. While another human
/// is in a live match it is `busy_fallback_secs` (≈2.5 human-vs-AI matches); when
/// nobody else is playing it is `solo_fallback_secs` (4 s). Without that split, two
/// players can share the arena for minutes and never meet — their cycles sit offset by
/// tens of seconds and a 4 s fallback cannot bridge the gap, so each is handed a bot
/// before the other can finish. Observed on prod 2026-08-03; the trace is in
/// `ArenaConfig::busy_fallback_secs`.
///
/// The delay is only ever paid when it can buy something. It applies solely while
/// somebody else is mid-match, and it is recomputed every pass — so if that player
/// finishes and does not return, the deadline drops back to the solo one instead of
/// stranding whoever is waiting.
async fn matchmaker_loop(
    mut rx: UnboundedReceiver<MatchmakerCommand>,
    config: ArenaConfig,
    registry: Arc<MatchRegistry>,
    db: Option<DbPool>,
) {
    info!(
        "matchmaker: started (advertise {}:{}, max {} matches)",
        config.advertise_host, config.udp_port, registry.max_matches
    );

    // Tickets waiting for an opponent, each with WHEN it started waiting. The
    // instant lives alongside the ticket rather than in a parallel structure so the
    // two cannot drift apart.
    //
    // This was a single `Option` — one waiting ticket, paired with whoever queued
    // next regardless of strength. A bracket needs somewhere to put the player you
    // decline to pair, so it needs a list (tracker #19). In practice this holds one
    // or two entries; the arena has a handful of players, not a lobby.
    let mut waiting: Vec<(TicketRequest, Instant)> = Vec::new();
    // Who queued recently, for the middle fallback tier. Not the same as `waiting`: a
    // player who queued, got a bot and is now mid-fight has LEFT `waiting` but is
    // exactly the person the next queuer should hold the door for.
    let mut arrivals: Vec<(Uuid, Instant)> = Vec::new();
    // Per-ticket earliest-fallback floor and the tier that set it, so a deadline can
    // never move backwards into the past when the arena empties out. See [`next_floor`].
    let mut floors: std::collections::HashMap<Uuid, (Instant, u8)> =
        std::collections::HashMap::new();
    loop {
        // If a ticket is already waiting, race the next command against its fallback
        // deadline; otherwise just block for the next command.
        // The queue's next deadline is the OLDEST waiting ticket's — it is the one
        // that has earned a fallback first.
        // Prune the arrival log before anyone reads it, so a long-idle arena cannot
        // keep quoting a player who left an hour ago.
        let window = Duration::from_secs(config.recent_window_secs);
        let cutoff = Instant::now();
        arrivals.retain(|(_, at)| cutoff.saturating_duration_since(*at) <= window);
        floors.retain(|tid, _| waiting.iter().any(|(t, _)| t.ticket_id == *tid));

        let oldest_ticket = waiting.iter().min_by_key(|(_, s)| *s);
        let oldest = oldest_ticket.map(|(_, s)| *s);
        let oldest_user = oldest_ticket.map(|(t, _)| t.user_id);
        let oldest_tid = oldest_ticket.map(|(t, _)| t.ticket_id);
        let next = if let Some(since) = oldest {
            // HUMANS FIRST. While anyone else is in a live match they are, by
            // definition, about to be free — so hold the queue open long enough to
            // catch them, rather than handing this player a bot they did not ask for.
            //
            // Recomputed on every pass, which is the point: if that other player
            // finishes and does NOT come back, `live_human_count()` drops to 0 and the
            // deadline collapses to `solo_fallback_secs`. Nobody is left waiting
            // minutes for a player who left.
            let recent = oldest_user
                .map(|me| others_recent(&arrivals, me, window, cutoff))
                .unwrap_or(0);
            // Everyone in `waiting` except the ticket whose deadline this is.
            let waiting_others = waiting.len().saturating_sub(1);
            let others_live = registry.live_human_count();
            let now = Instant::now();
            let tid = oldest_tid.expect("waiting is non-empty");
            let (floor, tier) = fallback_deadline(
                &config,
                &registry,
                since,
                recent,
                waiting_others,
                floors.get(&tid).copied(),
                now,
            );
            floors.insert(tid, (floor, tier));
            let deadline = floor;
            if now >= deadline {
                // Pop the oldest — the one whose deadline just fired.
                let idx = waiting
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, (_, s))| *s)
                    .map(|(i, _)| i)
                    .expect("waiting is non-empty");
                let (lone, since) = waiting.remove(idx);
                let waited = since.elapsed();
                // Don't spin up a bot match for a client that's already gone.
                if lone.rms.is_gone().await {
                    info!(
                        "matchmaker: waiting ticket {} abandoned (RMS gone) — dropped, no fallback",
                        lone.ticket_id
                    );
                    continue;
                }
                // LAST CALL. Before minting a bot, look at who is actually standing in
                // the queue. The bracket said "not yet" to these two; the deadline says
                // "now or never", and a human out of bracket beats a bot.
                //
                // Stale tickets are pruned on Enqueue but not here — nothing arrived to
                // trigger that pass — so re-check liveness, or we mint a ghost match
                // against a client that has gone.
                let mut live_tickets: Vec<(TicketRequest, Instant)> =
                    Vec::with_capacity(waiting.len());
                for (t, s) in std::mem::take(&mut waiting) {
                    if t.rms.is_gone().await {
                        info!(
                            "matchmaker: discarded stale waiting ticket {} (RMS gone)",
                            t.ticket_id
                        );
                    } else {
                        live_tickets.push((t, s));
                    }
                }
                waiting = live_tickets;

                let shape: Vec<(Option<Skill>, Instant)> =
                    waiting.iter().map(|(t, s)| (t.skill, *s)).collect();
                if let Some(idx) = last_call_partner(&shape, lone.skill, Instant::now()) {
                    let (partner, partner_since) = waiting.remove(idx);
                    info!(
                        "matchmaker: last call for ticket {} after {:.1}s — pairing with HUMAN {} out of bracket (they waited {:.1}s) rather than a bot",
                        lone.ticket_id,
                        waited.as_secs_f32(),
                        partner.ticket_id,
                        partner_since.elapsed().as_secs_f32(),
                    );
                    resolve(&registry, &config, &db, &[partner, lone], 0).await;
                    continue;
                }

                info!(
                    "matchmaker: no human opponent for ticket {} after {:.1}s ({} other human(s) in a live match) — solo fallback (vs bot)",
                    lone.ticket_id,
                    waited.as_secs_f32(),
                    others_live,
                );
                resolve(&registry, &config, &db, &[lone], 1).await;
                continue;
            }
            // Sleep to the deadline, but wake at least every FALLBACK_REEVALUATE so the
            // branch above can react to matches starting and ending. Also fixes a
            // latent bug: this used to be `sleep(solo_fallback_secs)` created fresh on
            // every iteration, so ANY unrelated command — another player's enqueue, a
            // cancel — silently restarted the timer. At 4 s that was invisible; at 230 s
            // it would mean a busy queue never falls back at all.
            let nap = (deadline - now).min(FALLBACK_REEVALUATE);
            tokio::select! {
                r = rx.recv() => r,
                _ = tokio::time::sleep(nap) => continue,
            }
        } else {
            rx.recv().await
        };
        let Some(cmd) = next else { break };

        let req = match cmd {
            MatchmakerCommand::Enqueue(req) => req,
            // Cancellation is routed through the actor so the ONLY owner of `waiting`
            // can dequeue the cancelled ticket. Before this, cancel was a no-op and a
            // cancelled ticket still zombie-resolved into a bot match on the fallback
            // timer — the client saw a `Succeeded` for a match it had abandoned. Match
            // on both ticket_id AND user_id so a cancel can only drop THAT user's ticket.
            MatchmakerCommand::Cancel { ticket_id, user_id } => {
                let before = waiting.len();
                waiting.retain(|(t, _)| !(t.ticket_id == ticket_id && t.user_id == user_id));
                if waiting.len() < before {
                    info!(
                        "matchmaker: cancelled waiting ticket {ticket_id} (user {user_id}) — dequeued"
                    );
                } else {
                    info!(
                        "matchmaker: cancel for ticket {ticket_id} — not in the queue (already resolved/gone)"
                    );
                }
                continue;
            }
        };

        info!(
            "matchmaker: ticket {} (user {})",
            req.ticket_id, req.user_id
        );
        arrivals.push((req.user_id, Instant::now()));
        record_match_queued(&db, req.ticket_id, req.user_id).await;
        // Push the captured 3-frame progression's first two frames now; the
        // `Succeeded` frame follows once the match resolves (pair or fallback). Sent to
        // the client's CURRENT live rms sender (re-fetched by `RmsHandle::send`).
        let _ = req
            .rms
            .send(MatchmakingMessage::Searching {
                ticket_id: req.ticket_id,
            })
            .await;
        let _ = req
            .rms
            .send(MatchmakingMessage::PotentialMatch {
                ticket_id: req.ticket_id,
            })
            .await;

        // Drop any waiting ticket whose client has gone. Pairing against a stale
        // ticket mints a ghost match the opponent never connects to — the emu-vs-pixel
        // failure where each device kept pairing with the other's cancelled ticket.
        let mut live: Vec<(TicketRequest, Instant)> = Vec::with_capacity(waiting.len());
        for (t, since) in std::mem::take(&mut waiting) {
            if t.rms.is_gone().await {
                info!(
                    "matchmaker: discarded stale waiting ticket {} (RMS gone)",
                    t.ticket_id
                );
            } else {
                live.push((t, since));
            }
        }
        waiting = live;

        // Pick an opponent inside the bracket. Among those that qualify, take the
        // CLOSEST in trophies — the bracket says who is allowed, this says who is
        // best. Ties and unknown skills fall back to the longest-waiting, so nobody
        // is starved by a stream of better-matched arrivals.
        let now = Instant::now();
        let mut best: Option<(usize, i64, Duration)> = None;
        for (i, (cand, since)) in waiting.iter().enumerate() {
            let waited = now.saturating_duration_since(*since);
            if !compatible(cand.skill, req.skill, waited) {
                continue;
            }
            let gap = match (cand.skill, req.skill) {
                (Some(a), Some(b)) => (a.trophies - b.trophies).abs(),
                _ => i64::MAX,
            };
            let better = match best {
                None => true,
                Some((_, best_gap, best_waited)) => {
                    gap < best_gap || (gap == best_gap && waited > best_waited)
                }
            };
            if better {
                best = Some((i, gap, waited));
            }
        }

        match best {
            Some((idx, gap, waited)) => {
                let (first, _) = waiting.remove(idx);
                info!(
                    "matchmaker: paired {} with {} (trophy gap {}, opponent waited {:.1}s)",
                    req.ticket_id,
                    first.ticket_id,
                    if gap == i64::MAX { -1 } else { gap },
                    waited.as_secs_f32(),
                );
                resolve(&registry, &config, &db, &[first, req], 0).await
            }
            None => {
                if !waiting.is_empty() {
                    info!(
                        "matchmaker: {} queued — {} other(s) waiting but none inside the bracket yet",
                        req.ticket_id,
                        waiting.len()
                    );
                }
                waiting.push((req, Instant::now()));
            }
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
            load_loadout(db, t.user_id, t.character_id),
        )
        .await
        {
            Ok(lo) => lo,
            Err(_) => {
                warn!(
                    "matchmaker: loadout load timed out (user {}) — starter",
                    t.user_id
                );
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
            let human_char_uuid: Option<String> = loadouts.get(0).map(|l| l.character_uuid.clone());
            for i in 0..bots {
                let lo = match tokio::time::timeout(
                    std::time::Duration::from_millis(1500),
                    load_loadout(db, ghost_id, None),
                )
                .await
                {
                    Ok(lo) => lo,
                    Err(_) => {
                        warn!(
                            "matchmaker: DEBUG ghost loadout load timed out (user {ghost_id}) — starter"
                        );
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

    // PRODUCTION BOT FILL (2026-07-03 solo-bot fix). Any bot slot NOT filled by a
    // configured debug ghost gets a REAL, COMPLETE, DISTINCT opponent character. A real
    // character has a non-empty op54 PROFILE, so the engine's broadcast emits the
    // opponent → the client RENDERS + BINDS the NPC (visible), dispatches the player's
    // attacks against it (killable), and the match-end result card carries a resolvable
    // winner (no post-match "stuck on loading" hang). Without this the bot slot falls to
    // loadout::starter() (empty profile) → the invisible/unkillable-bot bug. Bounded by
    // the same 1.5s timeout so a slow query never hangs the single matchmaker actor.
    if bots > 0 && loadouts.len() < tickets.len() + bots {
        let human_char_uuid = loadouts
            .get(0)
            .map(|l| l.character_uuid.clone())
            .unwrap_or_default();
        // The lone human's strength, so the bot is drawn from the same bracket a
        // human opponent would have had to satisfy (tracker #24). Read at enqueue by
        // `load_skill`; `None` when that lookup failed, which `pick_bot_index`
        // degrades to "any eligible bot" rather than refusing to start a match.
        let human_skill = tickets.first().and_then(|t| t.skill);
        while loadouts.len() < tickets.len() + bots {
            let bot = match tokio::time::timeout(
                std::time::Duration::from_millis(1500),
                load_bot_loadout(db, &human_char_uuid, human_skill, config, game_session_id),
            )
            .await
            {
                Ok(b) => b,
                Err(_) => {
                    warn!("matchmaker: bot loadout load timed out — empty starter bot");
                    crate::arena::combat::loadout::starter()
                }
            };
            info!(
                "matchmaker: solo bot slot {} → \"{}\" (char {}, profile {} B → {})",
                loadouts.len(),
                bot.display_name,
                bot.character_uuid,
                bot.profile_character_json.len(),
                if bot.profile_character_json.is_empty() {
                    "INVISIBLE (no complete bot available)"
                } else {
                    "opponent op54 PROFILE will broadcast"
                },
            );
            loadouts.push(bot);
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
    //
    // FIX 2 — UN-STICK on same-char rejection: previously the two tickets were left
    // silently unresolved, so each client hung at "determining server" until the 600s
    // solo-fallback timer fired (the `waiting` slot is empty after pairing, so neither
    // ticket stays in the actor — they just vanish). Now we send `MatchmakingFailed`
    // (ticketStatus "MatchmakingFailed", il2cpp-confirmed dump.cs 484188) to each
    // client immediately. The client's `RmsMatchmakingEvent.HasFailed()` / the
    // `PvpClientStatsCollector.MatchmakingFailed()` path surfaces this as an error
    // on the matchmaking screen, so the player sees a clear failure rather than an
    // infinite wait. Root cause of the same-char collapse is Fix 1 (device→char
    // binding instability); Fix 2 is the safety net when Fix 1's binding is still
    // incomplete (e.g. the prod DB hasn't been migrated yet or a device has never
    // been bound).
    if paired && bots == 0 {
        // Both halves of the identity are checked: the binding KEY (character_uuid,
        // what the client's GetPvpPlayer looks the avatar up by) and the VALUE it
        // dresses that avatar from (profile_character_json). Either one collapsing is
        // the same visible bug, so both are hard failures.
        let identity_check = check_paired_uuids_distinct(&loadouts)
            .and_then(|()| check_paired_profiles_present_and_distinct(&loadouts));
        if let Err(reason) = identity_check {
            warn!(
                "matchmaker: PAIRED-MATCH APPEARANCE COLLAPSE rejected (gsid {game_session_id}) — {reason} \
                 Sending MatchmakingFailed to both clients (Fix 2 un-stick). Root cause: two peers \
                 resolved to the SAME characters row — re-link each device to its own character via \
                 /arena (or wait for the source_wg_ip Fix 1 migration to restore lost bindings)."
            );
            for t in tickets {
                warn!(
                    "matchmaker: ticket {} → MatchmakingFailed (paired-match appearance guard, same char UUID)",
                    t.ticket_id
                );
                // Best-effort: if the client's RMS feed is already closed this is a no-op.
                let _ = t
                    .rms
                    .send(MatchmakingMessage::Failed {
                        ticket_id: t.ticket_id,
                    })
                    .await;
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
        // Send to the client's CURRENT live rms sender (re-fetched here, not the sender
        // captured at enqueue): the client reconnects its rms WS repeatedly, so a
        // captured sender would be a stale channel it no longer reads → the ONLY frame
        // carrying the arena address vanishes → permanent "determining server". If the
        // live sender is closed/absent, log it (the client is genuinely gone).
        if t.rms.send(succeeded).await.is_err() {
            // The match's capacity permit is held until both players connect; an
            // abandoned ticket leaks one slot until expiry (TODO: deadline sweep).
            warn!(
                "matchmaker: ticket {} — no live client RMS sender for Succeeded (client gone or reconnecting)",
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

/// The queueing player's level and trophies, for the pairing bracket.
///
/// Returns `None` rather than an error on any failure — no character, no database,
/// a malformed row. Matchmaking must degrade to "pair anyone" rather than refuse
/// to queue someone because a lookup went wrong.
async fn load_skill(
    app_state: &Arc<ServerGlobal>,
    user_id: Uuid,
    character_id: Option<Uuid>,
) -> Option<Skill> {
    use crate::schema::characters;
    use diesel::{ExpressionMethods, QueryDsl};
    use diesel_async::RunQueryDsl;

    let mut conn = app_state.db_pool.get().await.ok()?;
    let rows: Vec<(Uuid, serde_json::Value)> = characters::table
        .filter(characters::user_id.eq(user_id))
        .select((characters::id, characters::character))
        .load(&mut conn)
        .await
        .ok()?;
    let skill_of = |c: &serde_json::Value| {
        Some(Skill {
            level: c.get("level")?.as_i64()? as i32,
            trophies: c
                .get("matchmakingPvpTrophies")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        })
    };
    // The request DOES say which character is queueing — `matches/create`'s `playerId`.
    // Bracket on that one, so the character you are matched as is the character you
    // fight as. This used to take the max across all of a player's characters while
    // `load_loadout` took an arbitrary row, so the two could disagree.
    if let Some(want) = character_id {
        if let Some((_, c)) = rows.iter().find(|(id, _)| *id == want) {
            return skill_of(c);
        }
    }
    // No character named (or it is not this user's): strongest, matching
    // `pick_character`'s fallback so the two stay in agreement.
    rows.into_iter()
        .filter_map(|(_, c)| skill_of(&c))
        .max_by_key(|s| (s.trophies, s.level))
}

#[post("/api/matchmaking/v1/public/matches/create")]
pub async fn create_match(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    body: web::Json<CreateMatchRequest>,
) -> Result<Json<CreateMatchResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let ticket_id = Uuid::new_v4();

    // The RMS WebSocket must already be open — the client holds it from login. We
    // require it open at enqueue (so a client without a feed can't queue), but we hand
    // the matchmaker the `Arc<Session>` (not a cloned sender): the client reconnects
    // its rms WS repeatedly, so the matchmaker must re-fetch the CURRENT live sender at
    // resolve time (`RmsHandle::Session`) or `Succeeded` lands in a stale channel and
    // the client hangs at "determining server" (the stale-sender race).
    // 409-4-1 when there is no feed. NOTE: this being empty is not always the
    // client's fault — until 2026-07-30 a reconnecting socket's predecessor
    // blind-cleared the slot on teardown, so a client with a perfectly healthy
    // WebSocket got 409-4-1 on every match for the rest of its session. See
    // Session::clear_matchmaking_ws_if_owner.
    if !session.session.has_matchmaking_ws().await {
        log::warn!(
            "matchmaker: refusing ticket for user {} — no rms feed registered",
            session.session.user_id
        );
        return Err(BladeApiError::new(StatusCode::CONFLICT, 4, 1));
    }

    // Read the player's strength once, here, so the matchmaker actor stays
    // synchronous over its queue and never blocks pairing on a database round trip.
    // A failed lookup is not fatal: `compatible` treats an unknown skill as
    // matchable, so the worst case is the old behaviour for that one player.
    // `playerId` is the CHARACTER queueing, not the account — capture-confirmed: that
    // UUID appears 3,198 times in `/characters/{id}/...` paths in the corpus and never
    // where an account id belongs. We used to bind this body to `_body` and discard it.
    let character_id = body.player_id;
    let skill = load_skill(&app_state, session.session.user_id, character_id).await;

    app_state
        .arena
        .matchmaker_tx
        .send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id,
            user_id: session.session.user_id,
            character_id,
            rms: RmsHandle::Session(session.session.clone()),
            skill,
        }))
        .map_err(|_| BladeApiError::new(StatusCode::SERVICE_UNAVAILABLE, 4, 2))?;

    Ok(Json(CreateMatchResponse {
        r#match: MatchTicket {
            ticket_id,
            status: "QUEUED",
            port: 0,
        },
    }))
}

#[post("/api/matchmaking/v1/public/matches/{ticket_id}/cancel")]
pub async fn cancel_match(
    path: web::Path<Uuid>,
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
) -> Result<HttpResponse, BladeApiError> {
    let session = session.get_session_or_error()?;
    let ticket_id = path.into_inner();
    info!("matchmaker: cancel ticket {ticket_id}");
    // Route the cancel INTO the matchmaker actor so it actually DEQUEUES the ticket
    // from `waiting`. This was previously an acknowledged no-op, so a cancelled ticket
    // still zombie-resolved into a bot match on the solo-fallback timer (the client got
    // a `Succeeded`/"determining server" for a match it had abandoned). Scoped to this
    // user's id so a cancel can only drop that user's own waiting ticket. Best-effort:
    // if the actor's channel is gone the ticket can't be in-queue anyway.
    let _ = app_state
        .arena
        .matchmaker_tx
        .send(MatchmakerCommand::Cancel {
            ticket_id,
            user_id: session.session.user_id,
        });
    // Captured behavior: 200 with a literal `null` body.
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
        assert_ne!(
            psid_a, psid_b,
            "each player gets a distinct playerSessionId"
        );

        let gsid_s = gsid.to_string();
        let want_prefix = format!(
            "psess-{}",
            gsid_s.splitn(4, '-').take(3).collect::<Vec<_>>().join("-")
        );
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
        assert_ne!(
            suffix(psid_a),
            suffix(psid_b),
            "per-player suffixes are distinct"
        );
    }

    /// Two tickets enqueued back-to-back form ONE shared match — but a DB-less pair
    /// (both `load_loadout`s fall back to the empty-UUID `starter()`) is now REFUSED by
    /// the paired-match appearance guard (`docs/arena-appearance-bug-spec.md`): two
    /// empty `character_uuid`s would collapse both avatars onto the local PvpPlayer
    /// (the appearance-swap bug). No `Succeeded` is sent; capacity is returned. The
    /// un-stick test below (`same_char_pair_gets_failed_not_hung`) covers Fix 2 (the
    /// `Failed` message sent on the rejection path).
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
            bot_user_ids: Vec::new(),
            busy_fallback_secs: 230,
            recent_fallback_secs: 30,
            recent_window_secs: 300,
        };
        let (tx, rx) = unbounded_channel::<MatchmakerCommand>();
        tokio::spawn(matchmaker_loop(rx, config, registry.clone(), None));

        let (rms_a, mut recv_a) = unbounded_channel();
        let (rms_b, mut recv_b) = unbounded_channel();
        tx.send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            character_id: None,
            rms: RmsHandle::Direct(rms_a),
            skill: None,
        }))
        .unwrap();
        tx.send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            character_id: None,
            rms: RmsHandle::Direct(rms_b),
            skill: None,
        }))
        .unwrap();

        // No `Succeeded` arrives on either channel (the empty-UUID pair is refused).
        // (The `Failed` frames arrive before this timeout — see `same_char_pair_gets_failed_not_hung`.)
        let no_succeeded = |r: &Result<Option<MatchmakingMessage>, _>| {
            !matches!(r, Ok(Some(MatchmakingMessage::Succeeded { .. })))
        };
        // Drain up to 3 messages per side (Searching + PotentialMatch + Failed) and
        // confirm no Succeeded sneaks through.
        for _ in 0..3 {
            let got_a = tokio::time::timeout(Duration::from_millis(200), recv_a.recv()).await;
            assert!(
                no_succeeded(&got_a),
                "no Succeeded for an empty-UUID paired match (appearance guard)"
            );
            if matches!(got_a, Err(_)) {
                break;
            } // timeout = no more messages
        }
        for _ in 0..3 {
            let got_b = tokio::time::timeout(Duration::from_millis(200), recv_b.recv()).await;
            assert!(
                no_succeeded(&got_b),
                "no Succeeded for an empty-UUID paired match (appearance guard)"
            );
            if matches!(got_b, Err(_)) {
                break;
            }
        }
        // The capacity permit was returned (no match allocated), so all 4 are free.
        assert_eq!(
            registry.available_permits(),
            4,
            "the refused pair holds no capacity permit"
        );
    }

    /// Fix 2 — un-stick on same-char rejection: when the appearance guard rejects
    /// a same-character pair (both tickets resolve to the same empty-UUID starter
    /// loadout, as in the no-DB unit environment), each client must receive a
    /// `MatchmakingFailed` frame (ticketStatus "MatchmakingFailed",
    /// il2cpp-confirmed dump.cs 484188) rather than being left silently unresolved
    /// and hanging at "determining server" until the 600s solo-fallback timer.
    #[tokio::test]
    async fn same_char_pair_gets_failed_not_hung() {
        let registry = MatchRegistry::new(4);
        let config = ArenaConfig {
            advertise_host: "127.0.0.1".into(),
            udp_port: 7777,
            max_concurrent_matches: 4,
            max_queued_players: 64,
            solo_fallback_secs: 600, // long fallback — the Failed must arrive BEFORE it
            debug_ghost_user_id: None,
            bot_user_ids: Vec::new(),
            busy_fallback_secs: 230,
            recent_fallback_secs: 30,
            recent_window_secs: 300,
        };
        let (tx, rx) = unbounded_channel::<MatchmakerCommand>();
        tokio::spawn(matchmaker_loop(rx, config, registry.clone(), None));

        let tid_a = Uuid::new_v4();
        let tid_b = Uuid::new_v4();
        let (rms_a, mut recv_a) = unbounded_channel();
        let (rms_b, mut recv_b) = unbounded_channel();
        tx.send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id: tid_a,
            user_id: Uuid::new_v4(),
            character_id: None,
            rms: RmsHandle::Direct(rms_a),
            skill: None,
        }))
        .unwrap();
        tx.send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id: tid_b,
            user_id: Uuid::new_v4(),
            character_id: None,
            rms: RmsHandle::Direct(rms_b),
            skill: None,
        }))
        .unwrap();

        // Drain until Failed arrives or 1 s elapses (Searching + PotentialMatch come first).
        let failed_a = {
            let mut found: Option<MatchmakingMessage> = None;
            loop {
                match tokio::time::timeout(Duration::from_millis(1000), recv_a.recv()).await {
                    Ok(Some(msg)) if matches!(msg, MatchmakingMessage::Failed { .. }) => {
                        found = Some(msg);
                        break;
                    }
                    Ok(Some(_)) => {} // Searching / PotentialMatch — keep draining
                    _ => break,       // timeout or channel closed
                }
            }
            found
        };
        let failed_b = {
            let mut found: Option<MatchmakingMessage> = None;
            loop {
                match tokio::time::timeout(Duration::from_millis(1000), recv_b.recv()).await {
                    Ok(Some(msg)) if matches!(msg, MatchmakingMessage::Failed { .. }) => {
                        found = Some(msg);
                        break;
                    }
                    Ok(Some(_)) => {}
                    _ => break,
                }
            }
            found
        };
        assert!(
            matches!(failed_a, Some(MatchmakingMessage::Failed { ticket_id }) if ticket_id == tid_a),
            "client A must receive Failed (appearance guard un-stick, Fix 2); got {failed_a:?}"
        );
        assert!(
            matches!(failed_b, Some(MatchmakingMessage::Failed { ticket_id }) if ticket_id == tid_b),
            "client B must receive Failed (appearance guard un-stick, Fix 2); got {failed_b:?}"
        );
        // No match was allocated.
        assert_eq!(
            registry.available_permits(),
            4,
            "same-char rejection must not consume a capacity permit"
        );
    }

    /// A waiting ticket whose client has gone (its RMS feed closed — cancelled, timed
    /// out + retried, or disconnected) must NOT be bot-matched on the solo-fallback
    /// timer (nor paired against). Before the liveness fix it lingered in `waiting`, so
    /// the next ticket paired with the dead one → a ghost match the opponent never
    /// connected to (the emu-vs-pixel "opponent never connected; 1/2" failure).
    #[tokio::test]
    async fn stale_waiting_ticket_is_dropped_not_bot_matched() {
        let registry = MatchRegistry::new(4);
        let config = ArenaConfig {
            advertise_host: "127.0.0.1".into(),
            udp_port: 7777,
            max_concurrent_matches: 4,
            max_queued_players: 64,
            solo_fallback_secs: 1,
            debug_ghost_user_id: None,
            bot_user_ids: Vec::new(),
            busy_fallback_secs: 230,
            recent_fallback_secs: 30,
            recent_window_secs: 300,
        };
        let (tx, rx) = unbounded_channel::<MatchmakerCommand>();
        tokio::spawn(matchmaker_loop(rx, config, registry.clone(), None));

        // The client goes away immediately: drop the RMS receiver so is_closed() == true.
        let (rms_a, recv_a) = unbounded_channel();
        drop(recv_a);
        tx.send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            character_id: None,
            rms: RmsHandle::Direct(rms_a),
            skill: None,
        }))
        .unwrap();

        // Past the solo-fallback timer: the dead ticket is dropped, not bot-matched, so
        // no capacity permit is consumed.
        tokio::time::sleep(Duration::from_millis(1400)).await;
        assert_eq!(
            registry.available_permits(),
            4,
            "a dead waiting ticket must not consume a match permit (dropped, not bot-matched)"
        );
    }

    /// WIRING TEST for the reported bug — drives the real actor, not the helper.
    ///
    /// `last_call_partner` passing in isolation proves nothing: this file already warns
    /// that "a test of `fallback_delay` alone passes even if the loop never asks the
    /// registry anything, which is how this feature would ship green and do nothing at
    /// all." So assert on the frames the loop actually PUSHED.
    ///
    /// Two players outside the opening bracket (18 levels / 400 trophies apart) press
    /// Fight 300 ms apart, and the solo fallback is 1 s — the shape the testers hit,
    /// where the bracket has not widened once before the bot deadline arrives.
    ///
    /// With no DB both players resolve to the nil character UUID, so the *paired* path
    /// is refused by the appearance guard and pushes `Failed`, while the *bot* path
    /// pushes `Succeeded`. That difference is the probe:
    ///   - fixed:   last call pairs them -> `Failed` on both, no `Succeeded` anywhere.
    ///   - pre-fix: a bot each -> `Succeeded` on both and no `Failed` at all.
    #[tokio::test]
    async fn two_humans_queueing_together_get_each_other_not_two_bots() {
        let registry = MatchRegistry::new(4);
        let config = ArenaConfig {
            advertise_host: "127.0.0.1".into(),
            udp_port: 7777,
            max_concurrent_matches: 4,
            max_queued_players: 64,
            solo_fallback_secs: 1,
            debug_ghost_user_id: None,
            bot_user_ids: Vec::new(),
            busy_fallback_secs: 230,
            recent_fallback_secs: 30,
            recent_window_secs: 300,
        };
        let (tx, rx) = unbounded_channel::<MatchmakerCommand>();
        tokio::spawn(matchmaker_loop(rx, config, registry.clone(), None));

        let (rms_a, mut recv_a) = unbounded_channel();
        let (rms_b, mut recv_b) = unbounded_channel();

        tx.send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            character_id: None,
            rms: RmsHandle::Direct(rms_a),
            skill: Some(Skill {
                level: 62,
                trophies: 610,
            }),
        }))
        .unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;

        tx.send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            character_id: None,
            rms: RmsHandle::Direct(rms_b),
            skill: Some(Skill {
                level: 44,
                trophies: 210,
            }),
        }))
        .unwrap();

        async fn drain(
            rx: &mut tokio::sync::mpsc::UnboundedReceiver<MatchmakingMessage>,
        ) -> (bool, bool) {
            let (mut failed, mut succeeded) = (false, false);
            for _ in 0..6 {
                match tokio::time::timeout(Duration::from_millis(900), rx.recv()).await {
                    Ok(Some(MatchmakingMessage::Failed { .. })) => failed = true,
                    Ok(Some(MatchmakingMessage::Succeeded { .. })) => succeeded = true,
                    Ok(Some(_)) => continue, // Searching / PotentialMatch
                    _ => break,              // timeout or channel closed
                }
            }
            (failed, succeeded)
        }

        let (failed_a, succeeded_a) = drain(&mut recv_a).await;
        let (failed_b, succeeded_b) = drain(&mut recv_b).await;

        assert!(
            !succeeded_a && !succeeded_b,
            "a bot match was handed out while another HUMAN was waiting in the queue \
             (Succeeded a={succeeded_a} b={succeeded_b}) — the reported bug"
        );
        assert!(
            failed_a && failed_b,
            "the loop never attempted to pair them at last call (Failed a={failed_a} \
             b={failed_b}); without this the no-Succeeded assertion above could pass \
             simply because nothing happened at all"
        );
    }

    /// WIRING TEST for the recent tier. `others_recent` passing in isolation says
    /// nothing about whether the loop keeps an arrival log or ever reads it.
    ///
    /// A queues and takes a bot, so A is neither waiting nor in a live match — the hole
    /// between `solo` and `busy` where two Discord-coordinated players kept missing each
    /// other. B then queues alone. Under the old two-tier rule B is handed a bot after
    /// `solo_fallback_secs`; with the arrival log wired, B holds for
    /// `recent_fallback_secs` instead, which is the window A needs to come back.
    #[tokio::test]
    async fn a_recently_seen_player_makes_the_next_queuer_hold_the_door() {
        let registry = MatchRegistry::new(4);
        let config = ArenaConfig {
            advertise_host: "127.0.0.1".into(),
            udp_port: 7777,
            max_concurrent_matches: 4,
            max_queued_players: 64,
            solo_fallback_secs: 1,
            debug_ghost_user_id: None,
            bot_user_ids: Vec::new(),
            busy_fallback_secs: 230,
            recent_fallback_secs: 6,
            recent_window_secs: 300,
        };
        let (tx, rx) = unbounded_channel::<MatchmakerCommand>();
        tokio::spawn(matchmaker_loop(rx, config, registry.clone(), None));

        let (rms_a, _keep_a) = unbounded_channel();
        tx.send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            character_id: None,
            rms: RmsHandle::Direct(rms_a),
            skill: Some(Skill {
                level: 50,
                trophies: 400,
            }),
        }))
        .unwrap();

        // A falls back to a bot and leaves the queue. A is now "recent" but neither
        // waiting nor (with no connected ENet peer) live.
        tokio::time::sleep(Duration::from_millis(1500)).await;

        let (rms_b, mut recv_b) = unbounded_channel();
        tx.send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            character_id: None,
            rms: RmsHandle::Direct(rms_b),
            skill: Some(Skill {
                level: 50,
                trophies: 400,
            }),
        }))
        .unwrap();

        // Comfortably past B's solo deadline (1 s), comfortably inside the recent one (6 s).
        tokio::time::sleep(Duration::from_millis(3000)).await;

        let mut succeeded = false;
        while let Ok(msg) = recv_b.try_recv() {
            if matches!(msg, MatchmakingMessage::Succeeded { .. }) {
                succeeded = true;
            }
        }
        assert!(
            !succeeded,
            "B was handed a bot after ~1 s despite another human having queued moments \
             earlier — the arrival log is not reaching the deadline calculation"
        );
    }

    /// A CANCEL routed into the actor must DEQUEUE the waiting ticket so it never
    /// zombie-resolves on the solo-fallback timer. Before the fix, cancel was a no-op:
    /// the cancelled ticket stayed in `waiting` and still bot-matched after the timer,
    /// pushing a `Succeeded` for a match the client had abandoned. Here the waiting
    /// ticket is cancelled BEFORE the (short) fallback fires; past the timer no match
    /// was allocated (no permit consumed) and no `Succeeded` was sent.
    #[tokio::test]
    async fn cancel_dequeues_waiting_ticket() {
        let registry = MatchRegistry::new(4);
        let config = ArenaConfig {
            advertise_host: "127.0.0.1".into(),
            udp_port: 7777,
            max_concurrent_matches: 4,
            max_queued_players: 64,
            solo_fallback_secs: 1,
            debug_ghost_user_id: None,
            bot_user_ids: Vec::new(),
            busy_fallback_secs: 230,
            recent_fallback_secs: 30,
            recent_window_secs: 300,
        };
        let (tx, rx) = unbounded_channel::<MatchmakerCommand>();
        tokio::spawn(matchmaker_loop(rx, config, registry.clone(), None));

        let tid = Uuid::new_v4();
        let uid = Uuid::new_v4();
        let (rms, mut recv) = unbounded_channel();
        tx.send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id: tid,
            user_id: uid,
            character_id: None,
            rms: RmsHandle::Direct(rms),
            skill: None,
        }))
        .unwrap();

        // Let Searching/PotentialMatch enqueue, then cancel BEFORE the 1s fallback.
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send(MatchmakerCommand::Cancel {
            ticket_id: tid,
            user_id: uid,
        })
        .unwrap();

        // Past the fallback timer: the cancelled ticket must NOT have bot-matched.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            registry.available_permits(),
            4,
            "a cancelled ticket must not consume a match permit (dequeued, not zombie-resolved)"
        );
        // Drain the channel: only Searching + PotentialMatch, never a Succeeded.
        while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(50), recv.recv()).await
        {
            assert!(
                !matches!(msg, MatchmakingMessage::Succeeded { .. }),
                "a cancelled ticket must never receive a Succeeded frame; got {msg:?}"
            );
        }
    }

    /// A cancel that does NOT match the waiting ticket's (ticket_id, user_id) must
    /// leave the waiting ticket in place — a cancel can only drop its OWN ticket. Here
    /// user A queues, a spurious cancel for a different ticket/user arrives, and A still
    /// bot-matches on the fallback timer (permit consumed).
    #[tokio::test]
    async fn cancel_does_not_drop_a_different_users_ticket() {
        let registry = MatchRegistry::new(4);
        let config = ArenaConfig {
            advertise_host: "127.0.0.1".into(),
            udp_port: 7777,
            max_concurrent_matches: 4,
            max_queued_players: 64,
            solo_fallback_secs: 1,
            debug_ghost_user_id: None,
            bot_user_ids: Vec::new(),
            busy_fallback_secs: 230,
            recent_fallback_secs: 30,
            recent_window_secs: 300,
        };
        let (tx, rx) = unbounded_channel::<MatchmakerCommand>();
        tokio::spawn(matchmaker_loop(rx, config, registry.clone(), None));

        let tid = Uuid::new_v4();
        let uid = Uuid::new_v4();
        let (rms, _recv) = unbounded_channel();
        tx.send(MatchmakerCommand::Enqueue(TicketRequest {
            ticket_id: tid,
            user_id: uid,
            character_id: None,
            rms: RmsHandle::Direct(rms),
            skill: None,
        }))
        .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        // A cancel for someone else's ticket must not touch A's waiting ticket.
        tx.send(MatchmakerCommand::Cancel {
            ticket_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
        })
        .unwrap();

        // Past the fallback timer: A still bot-matched (a solo bot needs no DB — the
        // starter bot fill allocates a permit), so one permit is consumed.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            registry.available_permits(),
            3,
            "a spurious cancel must not dequeue another user's ticket — A still bot-matches"
        );
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
        assert!(
            !is_self_match(human, ""),
            "empty ghost UUID ⇒ not a self-match"
        );
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
        assert!(
            check_paired_uuids_distinct(&ok).is_ok(),
            "distinct non-empty UUIDs must pass"
        );

        // Two equal non-empty UUIDs → rejected (both peers resolved to the same row →
        // appearance collapse). This is the WolfWalker-vs-Flappety reported symptom.
        let same = vec![
            with_uuid("38c987fd-c42b-4ea6-b869-c8d4c03055f9", "WolfWalker"),
            with_uuid("38c987fd-c42b-4ea6-b869-c8d4c03055f9", "Flappety"),
        ];
        let err = check_paired_uuids_distinct(&same).expect_err("shared UUID must be rejected");
        assert!(
            err.contains("share character_uuid"),
            "rejection names the shared-UUID collapse: {err}"
        );

        // An empty UUID (a starter() fallback on a slow load_loadout) → rejected: its
        // avatar propId4 would be "" → can't bind a distinct opponent, drops the profile.
        let empty = vec![
            with_uuid("38c987fd-c42b-4ea6-b869-c8d4c03055f9", "Flappety"),
            with_uuid("", "DegradedToStarter"),
        ];
        let err = check_paired_uuids_distinct(&empty).expect_err("empty UUID must be rejected");
        assert!(
            err.contains("EMPTY character_uuid"),
            "rejection names the empty-UUID collapse: {err}"
        );
    }

    /// The symmetric half of the guard: distinct `character_uuid`s are necessary but
    /// NOT sufficient. The op54 PROFILE is the blob the client dresses the avatar
    /// from, and a bare `loadout::starter()` fallback carries an EMPTY one — which
    /// `broadcast_profiles` skips, so the opponent's body keeps the local character's
    /// appearance even though every UUID was distinct.
    #[test]
    fn paired_profile_presence_guard() {
        use crate::arena::combat::loadout::starter;
        let fighter = |uuid: &str, name: &str, profile: &str| {
            let mut l = starter();
            l.character_uuid = uuid.to_string();
            l.display_name = name.to_string();
            l.profile_character_json = profile.to_string();
            l
        };
        const U1: &str = "38c987fd-c42b-4ea6-b869-c8d4c03055f9";
        const U2: &str = "1131a037-716c-49cc-b165-32d8ddc14f49";

        // Distinct UUIDs AND distinct non-empty profiles → OK.
        let ok = vec![
            fighter(U1, "Flappety", r#"{"id":"38c987fd","name":"Flappety"}"#),
            fighter(U2, "Blank", r#"{"id":"1131a037","name":"Blank"}"#),
        ];
        assert!(check_paired_profiles_present_and_distinct(&ok).is_ok());

        // Distinct UUIDs but one profile EMPTY (a degraded starter() fallback) →
        // rejected: this passes the UUID guard yet still collapses appearance.
        let degraded = vec![
            fighter(U1, "Flappety", r#"{"id":"38c987fd","name":"Flappety"}"#),
            fighter(U2, "DegradedToStarter", ""),
        ];
        assert!(
            check_paired_uuids_distinct(&degraded).is_ok(),
            "the UUID guard alone does NOT catch this — that is the point of the second guard"
        );
        let err = check_paired_profiles_present_and_distinct(&degraded)
            .expect_err("an empty profile must be rejected");
        assert!(
            err.contains("EMPTY profile_character_json"),
            "rejection names the empty-profile collapse: {err}"
        );

        // Two identical profiles → both clients dress both avatars from one blob.
        let shared = vec![
            fighter(U1, "Flappety", r#"{"id":"38c987fd","name":"Flappety"}"#),
            fighter(U2, "WolfWalker", r#"{"id":"38c987fd","name":"Flappety"}"#),
        ];
        let err = check_paired_profiles_present_and_distinct(&shared)
            .expect_err("an identical profile must be rejected");
        assert!(
            err.contains("IDENTICAL profile_character_json"),
            "rejection names the shared-profile collapse: {err}"
        );
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
        character.completed_quests =
            json!({ "q1": { "completed": true }, "q2": { "completed": true } });
        character.global_shop_offers = json!([{ "offerId": "x", "price": 100 }]);
        character.challenge_season = CharacterChallengeSeason {
            current_session_id: Some(Uuid::new_v4()),
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
        assert_eq!(
            obj.get("id").and_then(|i| i.as_str()),
            Some(id.to_string().as_str())
        );
        assert_eq!(obj.get("name").and_then(|n| n.as_str()), Some("Opponent"));
    }
}
