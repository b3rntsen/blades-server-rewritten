//! Authoritative arena combat engine.
//!
//! Arena PvP is **server-authoritative**: the client sends only *inputs*
//! (`PlayerCombatInputPosition`, `PlayerCombatInputActivate`, a c2s
//! `CombatSwipeInfo` swipe-input, and `RequestExecuteAbility` carrying an ability
//! instance UUID with an implicit target). The server simulates the fight and
//! tells each client what happened — damage (`ReceiveDamage`, s2c, `netRole =
//! Authority`), state changes, status effects, round/match flow. This module is
//! that simulation; the byte-level wire codec lives in `arena_proto::netdata`.
//!
//! ## Wire-protocol findings (reverse-engineered from prod session 293, the
//! June-6 build that matches `reference/apk/blades.apk` — the client we serve)
//!
//! - **`user_data[1]` is the NetTransport `MessageType`, NOT the GameMessageId.**
//!   The DB `opcode` column stored `user_data[1]`, so a value like "50" there is
//!   a *carrier type*, not `ReceiveDamage`. Real dispatch is structural (e.g. the
//!   `70 77` fingerprint for the op-54 carrier; propId 3 inside a damage body).
//!   Markers: `0xBE` s2c, `0x84` c2s/init, `0xAC` game-state.
//!
//! - **Match flow is a stateName state machine**, server-driven, client-echoed —
//!   NOT the `PlayerWelcome`/`PlayerSpawnAvatar` flow the old placeholder FSM
//!   assumed (those opcodes never appear in s293). The flow rides as an op-54
//!   carrier (`BE 36 … <firstPropId 0x4F s2c / 0x50 c2s> <u16-LE len> <ASCII>`)
//!   on a flow-controller net object. Observed states, in order, per match:
//!     `BackendMatchCreated` → repeated `StateTimeout` (a periodic **s2c
//!     heartbeat**, partly echoed c2s — this is why the engine needs a tick) →
//!     `RoundEnd` → `NextState` → (next round) → … (s293 = ~17 matches).
//!   See [`state::FlowState`].
//!
//! - **Loadout** (`OpponentLoadout` / `EquipAbilitiesAndConsumables`) rides the
//!   `0x84` / `0xAC` channels in an obfuscated form, later than match-create; the
//!   server's source of truth is the imported character, not these frames.
//!
//! ## Module layout (built out across phases A→C)
//! - [`state`] — per-match / per-fighter authoritative state + the protocol enums.
//! - `messages` (Phase A+) — s2c builders over `arena_proto::netdata`.
//! - `lifecycle` (Phase A+) — the [`state::FlowState`] machine.
//! - `resolve` (Phase B+) — input → hit/damage/ability/block resolution.
//! - `loadout` (Phase A+) — build a [`state::Fighter`] from a `CompleteCharacter`.

// Temporary while the engine is built out phase by phase: the typed state model
// lands before the code that reads every field. Remove once `resolve`/`messages`
// consume them (Phase B/C).
#![allow(dead_code)]

pub mod damage;
pub mod engine;
pub mod gamedata;
pub mod input;
pub mod loadout;
pub mod messages;
pub mod perks;
pub mod messages_state;
pub mod resolve;
pub mod state;
pub mod tables;

// Offline reproduction-differential test against retail capture s506 (round-start).
// Test-only: no production code, just drives the engine over s506's timing and
// diffs our s2c protocol sequence against the captured one.
// End-to-end proof that each PERK changes a fight. Differential by construction:
// every case runs perked and unperked and asserts the gap, so it cannot pass
// against an engine that resolves perks and applies them nowhere.
#[cfg(test)]
mod perks_effect;

#[cfg(test)]
mod roundtrip_s506;

// Offline reproduction-differential test for the DAMAGE model against s506 (per-hit
// magnitudes / combo ramp / asymmetric block / poison amp / paralyse threshold).
// Test-only: replays the §2a damage table through RetailDamageModel + Fighter and
// diffs computed vs recorded damage. The companion to `roundtrip_s506` (protocol).
#[cfg(test)]
mod roundtrip_s506_damage;

pub use engine::MatchInstance;
pub use state::Loadout;

/// Longest production debug-hold window accepted at startup.
///
/// The hold deliberately disables both the live-round transition and the normal
/// stuck-match sweep.  An unbounded flag is therefore an arena outage waiting for
/// the next container recreation: an old container keeps its original environment,
/// then a routine release suddenly reads the forgotten flag and every subsequent
/// match freezes at "Setting Up".  Keep this short and self-expiring.
const DEBUG_HOLD_MAX_WINDOW: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// **DEBUG.** Remaining hold time when `ARENA_DEBUG_HOLD` is truthy *and*
/// `ARENA_DEBUG_HOLD_UNTIL_UNIX` is a near-future Unix timestamp.
///
/// When ON, a connected match is FROZEN at the round-start: the full round-start
/// burst is still sent (the FSM reaches `BackendMatchCreated`), but it never
/// advances to the live combat round (`StateTimeout`), no bot swings, and the
/// match registry will not time-out/sweep a solo (under-capacity) peer — giving a
/// bounded window to hand-inject s2c frames via `/arena/debug/inject` and watch the
/// client.  The deadline is mandatory and may be at most 30 minutes away;
/// the engine and registry also re-check the monotonic deadline while running, so
/// an already-held match resumes and becomes sweepable when the window expires.
///
/// Requiring the second, expiring value is intentional.  A legacy
/// `ARENA_DEBUG_HOLD=1` left in `arena.env` is ignored instead of turning a later,
/// unrelated production restart into a global arena freeze.
pub fn debug_hold_window() -> Option<std::time::Duration> {
    let hold = std::env::var("ARENA_DEBUG_HOLD").ok();
    let until = std::env::var("ARENA_DEBUG_HOLD_UNTIL_UNIX").ok();
    debug_hold_window_at(
        hold.as_deref(),
        until.as_deref(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
}

fn debug_hold_window_at(
    hold: Option<&str>,
    until_unix: Option<&str>,
    now_unix: u64,
) -> Option<std::time::Duration> {
    let enabled = hold.is_some_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off" | ""
        )
    });
    if !enabled {
        return None;
    }

    let Some(until) = until_unix.and_then(|v| v.trim().parse::<u64>().ok()) else {
        log::error!(
            "ARENA_DEBUG_HOLD ignored: set ARENA_DEBUG_HOLD_UNTIL_UNIX to a Unix timestamp no more than 30 minutes ahead"
        );
        return None;
    };
    let Some(seconds) = until.checked_sub(now_unix) else {
        log::warn!("ARENA_DEBUG_HOLD ignored: ARENA_DEBUG_HOLD_UNTIL_UNIX has expired");
        return None;
    };
    if seconds == 0 || seconds > DEBUG_HOLD_MAX_WINDOW.as_secs() {
        log::error!(
            "ARENA_DEBUG_HOLD ignored: deadline must be 1..={} seconds ahead (got {seconds})",
            DEBUG_HOLD_MAX_WINDOW.as_secs(),
        );
        return None;
    }
    Some(std::time::Duration::from_secs(seconds))
}

#[cfg(test)]
mod debug_hold_tests {
    use super::{debug_hold_window_at, DEBUG_HOLD_MAX_WINDOW};

    #[test]
    fn stale_boolean_alone_cannot_freeze_a_restarted_server() {
        assert_eq!(debug_hold_window_at(Some("1"), None, 1_000), None);
        assert_eq!(debug_hold_window_at(Some("true"), Some("bad"), 1_000), None);
    }

    #[test]
    fn hold_requires_a_short_future_deadline() {
        assert_eq!(debug_hold_window_at(Some("1"), Some("999"), 1_000), None);
        assert_eq!(debug_hold_window_at(Some("1"), Some("1000"), 1_000), None);
        assert_eq!(
            debug_hold_window_at(Some("1"), Some("1001"), 1_000),
            Some(std::time::Duration::from_secs(1))
        );
        assert_eq!(
            debug_hold_window_at(Some("yes"), Some("2800"), 1_000),
            Some(DEBUG_HOLD_MAX_WINDOW)
        );
        assert_eq!(debug_hold_window_at(Some("1"), Some("2801"), 1_000), None);
    }

    #[test]
    fn false_values_ignore_even_a_valid_deadline() {
        for value in [None, Some(""), Some("0"), Some("false"), Some("NO"), Some("off")] {
            assert_eq!(debug_hold_window_at(value, Some("1100"), 1_000), None);
        }
    }
}
