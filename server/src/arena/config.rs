//! Arena subsystem configuration. Parsed from env vars (with sane defaults) so
//! it can be tuned on low-end hardware without touching the CLI. The
//! `max_*` caps are enforced once the UDP match layer lands (milestone c).

use std::env;

#[derive(Clone, Debug)]
#[allow(dead_code)] // max_* fields are wired up by the UDP/match layer (milestone c)
pub struct ArenaConfig {
    /// Host advertised to the client in `MatchmakingSucceeded.address` — the
    /// arena UDP endpoint the client will dial.
    pub advertise_host: String,
    /// UDP port advertised to the client.
    pub udp_port: u16,
    /// Cap on simultaneous live matches (low-end hardware bound).
    pub max_concurrent_matches: usize,
    /// Cap on queued matchmaking tickets before `create` returns 503.
    pub max_queued_players: usize,
}

impl ArenaConfig {
    pub fn from_env() -> Self {
        fn parse<T: std::str::FromStr>(key: &str, default: T) -> T {
            env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
        }
        ArenaConfig {
            advertise_host: env::var("ARENA_ADVERTISE_HOST")
                .unwrap_or_else(|_| "127.0.0.1".to_string()),
            udp_port: parse("ARENA_UDP_PORT", 7777),
            max_concurrent_matches: parse("ARENA_MAX_MATCHES", 4),
            max_queued_players: parse("ARENA_MAX_QUEUED", 64),
        }
    }
}
