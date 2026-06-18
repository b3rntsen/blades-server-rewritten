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
pub mod input;
pub mod loadout;
pub mod messages;
pub mod resolve;
pub mod state;
pub mod tables;

// Offline reproduction-differential test against retail capture s506 (round-start).
// Test-only: no production code, just drives the engine over s506's timing and
// diffs our s2c protocol sequence against the captured one.
#[cfg(test)]
mod roundtrip_s506;

pub use engine::MatchInstance;
pub use state::Loadout;
