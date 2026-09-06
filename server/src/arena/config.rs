//! Arena subsystem configuration. Parsed from env vars (with sane defaults) so
//! it can be tuned on low-end hardware without touching the CLI. The
//! `max_*` caps are enforced once the UDP match layer lands (milestone c).

use std::env;

use uuid::Uuid;

#[derive(Clone, Debug)]
#[allow(dead_code)] // max_* fields are wired up by the UDP/match layer (milestone c)
pub struct ArenaConfig {
    /// Host advertised to the client in `MatchmakingSucceeded.address` — the
    /// arena UDP endpoint the client will dial.
    pub advertise_host: String,
    /// Host advertised to clients that did NOT arrive through the WireGuard
    /// tunnel — the VPN-free build, reaching us over the public internet.
    ///
    /// `None` means "there is no public path", and everyone is told the tunnel
    /// address exactly as before. That is the safe default: a deployment that
    /// forgets to set this keeps working for VPN players instead of quietly
    /// publishing an address it does not serve.
    ///
    /// Kept SEPARATE rather than replacing `advertise_host`, because pointing
    /// tunnel clients at a public address would route their arena traffic
    /// outside the tunnel — and that traffic is the capture.
    pub public_advertise_host: Option<String>,
    /// UDP port advertised to the client.
    pub udp_port: u16,
    /// Cap on simultaneous live matches (low-end hardware bound).
    pub max_concurrent_matches: usize,
    /// Cap on queued matchmaking tickets before `create` returns 503.
    pub max_queued_players: usize,
    /// Seconds a lone matchmaking ticket waits for a human opponent before it falls
    /// back to a solo match against a bot. Tunable via ARENA_SOLO_FALLBACK_SECS:
    /// shorter = a solo tester gets a bot fight sooner; longer = a wider window for
    /// two near-simultaneous players to PAIR (coordinated taps pair instantly either
    /// way, since the 2nd ticket arrives while the 1st is waiting).
    ///
    /// This is deliberately SHORT (default 4s): the frame carrying the arena server
    /// address (`MatchmakingSucceeded`) is only sent once a ticket RESOLVES, so a lone
    /// player stares at "determining server" for exactly this long before the bot
    /// fallback fires. A real second human still pairs INSTANTLY within the window
    /// (the 2nd ticket arrives while the 1st waits), so keeping a brief window costs
    /// coordinated pairs nothing while un-sticking the common solo case fast.
    pub solo_fallback_secs: u64,
    /// **DEBUG (`ARENA_DEBUG_GHOST`).** When set to an arena `characters.user_id`
    /// UUID, the **solo-fallback** match (one lone human → vs bot) loads THAT
    /// character's real loadout into the 2nd fighter (slot 1) instead of the empty
    /// `starter()`. A real loadout has a non-empty `profile_character_json`, so the
    /// engine's existing `broadcast_profiles` emits the opponent's op54 PROFILE
    /// (GameMessageId 35) — the frame that flips the client's `ClientChecklist`
    /// `OpponentLoadoutReady` and crosses "Connecting…" → "Setting up…". Without it,
    /// the bot falls back to `starter()` (empty profile) and the profile is skipped
    /// → permanent "Connecting…" (capture-proven, docs/arena-ghost-gap-analysis.md).
    /// `None` when unset / unparseable → unchanged (today's empty-starter bot). Does
    /// NOT affect a real PvP pair (both players upload their own profiles).
    pub debug_ghost_user_id: Option<Uuid>,
    /// Roster of arena `characters.user_id` UUIDs to use as solo-match BOT opponents
    /// (env `ARENA_BOT_USER_IDS`, comma-separated). When set, a solo-fallback bot loads
    /// one of these (COMPLETE + distinct from the human, rotated by gsid). When EMPTY,
    /// the bot is instead any random COMPLETE character in the DB. Either way the bot
    /// gets a non-empty op54 PROFILE → the opponent is visible/bindable, killable, and
    /// the match-end card resolves (the 2026-07-03 invisible-bot / post-match-hang fix).
    /// Unlike `debug_ghost_user_id` this is the PRODUCTION bot path (not a debug crutch).
    pub bot_user_ids: Vec<Uuid>,
    /// How long a queued player waits for a HUMAN before falling back to a bot, when
    /// **somebody else is already in a live match** (env `ARENA_BUSY_FALLBACK_SECS`).
    ///
    /// `solo_fallback_secs` (4 s) is right when a player is genuinely alone, and wrong
    /// the moment two are around. Observed on prod 2026-08-03, two players for six
    /// minutes, never once matched with each other:
    ///
    /// ```text
    ///   20:38:31  A queues → 20:38:35 bot   (match ends 20:39:56)
    ///   20:39:25  B queues → 20:39:29 bot   (match ends 20:41:19)
    ///   20:41:03  A queues → 20:41:07 bot   (match ends 20:42:25)
    ///   20:41:49  B queues → 20:41:53 bot   (match ends 20:43:43)
    ///   20:42:43  A queues → 20:42:47 bot
    /// ```
    ///
    /// Their cycles are offset by ~50 s, and a 4 s fallback is far too short to bridge
    /// that: each is handed a bot before the other can possibly finish. Waiting longer
    /// than one human-vs-AI match guarantees the offset is absorbed — whoever queues
    /// second arrives while the first is still holding the queue open, and they pair.
    ///
    /// The default is **230 s ≈ 2.5×** the measured mean human-vs-AI match of 92.6 s
    /// (the five matches above: 81, 110, 78, 110, 84). That is "150 % longer", inside
    /// the 100–200 % the owner asked for.
    ///
    /// The cost is bounded and paid only when it can help: the delay applies solely
    /// while another human is in a live match, and collapses back to
    /// `solo_fallback_secs` the moment nobody is (see `matchmaker_loop`). A player who
    /// really is alone never waits longer than they do today.
    pub busy_fallback_secs: u64,
    /// How long a queued player waits for a HUMAN before falling back to a bot, when
    /// **more than one human has queued recently** but nobody is in a live match right
    /// now (env `ARENA_RECENT_FALLBACK_SECS`, default 30 s).
    ///
    /// The third tier, and it exists because the other two leave a hole.
    /// `busy_fallback_secs` only applies while somebody is *mid-match*; the moment they
    /// finish, `live_human_count()` drops to 0 and the deadline collapses back to
    /// `solo_fallback_secs` (4 s). But a player between fights — results card, menus,
    /// re-queuing — is not in a match and is exactly the partner you want to wait for.
    /// Two people coordinating on Discord kept landing in that hole.
    ///
    /// 30 s is chosen against `matchmaker::bracket_for`: the bracket goes unlimited at
    /// 30 s, so this is the first tier at which the whole schedule is reachable. Below
    /// it the later widening steps are dead code.
    pub recent_fallback_secs: u64,
    /// How far back to look for other humans when deciding whether
    /// `recent_fallback_secs` applies (env `ARENA_RECENT_WINDOW_SECS`, default 300 s).
    ///
    /// Five minutes spans a fight plus the results card plus a re-queue (measured
    /// human-vs-AI matches: 81, 110, 78, 110, 84 s), so two players trading fights stay
    /// "recent" to each other across the whole cycle.
    pub recent_window_secs: u64,
}

impl ArenaConfig {
    pub fn from_env() -> Self {
        fn parse<T: std::str::FromStr>(key: &str, default: T) -> T {
            env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
        }
        ArenaConfig {
            advertise_host: env::var("ARENA_ADVERTISE_HOST")
                .unwrap_or_else(|_| "127.0.0.1".to_string()),
            public_advertise_host: env::var("ARENA_ADVERTISE_HOST_PUBLIC")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            udp_port: parse("ARENA_UDP_PORT", 7777),
            max_concurrent_matches: parse("ARENA_MAX_MATCHES", 16),
            max_queued_players: parse("ARENA_MAX_QUEUED", 64),
            // SHORT by default (4s): the arena address only ships on resolve, so this
            // is the felt "determining server" wait for a solo player. A genuine 2nd
            // human still pairs instantly within the window (its ticket arrives while
            // the 1st waits). Bump ARENA_SOLO_FALLBACK_SECS to widen the pairing window.
            solo_fallback_secs: parse("ARENA_SOLO_FALLBACK_SECS", 4),
            busy_fallback_secs: parse("ARENA_BUSY_FALLBACK_SECS", 230),
            recent_fallback_secs: parse("ARENA_RECENT_FALLBACK_SECS", 30),
            recent_window_secs: parse("ARENA_RECENT_WINDOW_SECS", 300),
            // DEBUG ghost opponent (off when unset / unparseable → normal bot).
            debug_ghost_user_id: env::var("ARENA_DEBUG_GHOST")
                .ok()
                .and_then(|s| Uuid::parse_str(s.trim()).ok()),
            // Production solo-bot roster (comma-separated user_id UUIDs). Empty → any
            // random COMPLETE character in the DB is used as the bot.
            bot_user_ids: env::var("ARENA_BOT_USER_IDS")
                .ok()
                .map(|s| {
                    s.split(',')
                        .filter_map(|p| Uuid::parse_str(p.trim()).ok())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}
