//! The per-match combat engine — replaces the old placeholder `MatchInstance`.
//!
//! Owns a [`MatchCombat`] (authoritative state) and exposes the two entry points
//! the match layer drives:
//!   - [`MatchInstance::on_c2s`] — an inbound decrypted client message.
//!   - [`MatchInstance::on_tick`] — the per-loop tick (server-initiated messages:
//!     the flow-control state machine, plus DoT/cooldown/round logic in Phase C).
//!
//! Both return `(target_slot, full_user_data)` pairs — the **complete** decrypted
//! s2c payload (`0xBE ‖ MessageType ‖ body`), which the match layer encrypts under
//! the target peer's key. (The old tuple returned a bare body + a separate op byte;
//! returning the full payload is honest about MessageType-vs-GameMessageId and lets
//! `messages::*` builders own the framing.)

use std::time::{Duration, Instant};

use arena_proto::{GameMessageId, NetDataValue};
use log::{debug, info};

use super::messages;
use super::resolve;
use super::state::{Fighter, FlowState, Loadout, MatchCombat, MatchState, NetRole};

/// DIAGNOSTIC: lowercase-hex a byte slice for the op58/op54 wire-byte logging.
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// DIAGNOSTIC: render the parsed op58 NetData as `propId=value` Longs (hex+dec) so
/// the live c2s op58 can be diffed against the retail clock0/token decode.
fn fmt_netdata_longs(p: &arena_proto::NetDataParse) -> String {
    let mut parts = Vec::new();
    for (pid, v) in &p.props {
        match v {
            NetDataValue::Long(x) => parts.push(format!("p{pid}=Long(0x{:016x}={x})", *x as u64)),
            other => parts.push(format!("p{pid}={other:?}")),
        }
    }
    format!("[{}] ok={}", parts.join(", "), p.ok)
}

/// Carrier MessageType (`user_data[1]`) of the op58 match-CLOCK message — the
/// round-start clock-sync. The client SENDS this c2s on connect (two Longs:
/// `[clock0=0, token]`) and BLOCKS in MatchState `AwaitingClientBackendSynchronization`
/// until the server replies `op58 [server_clock_ticks, token]` echoing the SAME
/// token back to that one client — only then does it upload its loadout (op54).
/// Our prior bug shipped op58 only as an UNSOLICITED broadcast (no token echo) and
/// dropped the client's c2s op58 in `resolve` (early-return off StateTimeout), so
/// the client hung at "Connecting…". [capture-proven from s506.]
const CARRIER_CLOCK: u8 = 0x3a; // 58
/// Cadence of the `StateTimeout` flow heartbeat (server→client keepalive while a
/// phase runs). Captured cadence is sub-second; tunable.
const HEARTBEAT: Duration = Duration::from_millis(500);
/// Hold in the `Spawning` phase between the spawn/profile burst and the
/// `BackendMatchCreated` flow state. Retail staggers these ~4s (s506: spawns
/// 05:05:36, BackendMatchCreated 05:05:40); announcing the match in the same tick as
/// the spawns preempts the client's loadout-upload handshake → "Connecting" hang.
const SPAWN_HANDSHAKE_HOLD: Duration = Duration::from_secs(4);
/// Stagger between the Match-object spawn (`WaitingForPlayers`=3) and the
/// `InitialPlayerSetup`(4) update. Retail s506 obj 123: state 3 @05:05:36 → state 4
/// @05:05:37 (~1s). Both 3 and 4 must be SEEN by the client (it binds players across
/// them); a small hold lets it process the spawn before the 4 update.
const MATCH_SETUP_STAGGER: Duration = Duration::from_secs(1);
/// `CurrentMatchStateTimeout` (Match propId6) sent with the `WaitingForPlayers`(3)
/// spawn / `InitialPlayerSetup`(4) update — capture values from s506 (20s / 30s). The
/// client uses it for the lobby countdown only; not the binding gate.
const MATCH_STATE_WAIT_TIMEOUT: f32 = 20.0;
const MATCH_STATE_SETUP_TIMEOUT: f32 = 30.0;
/// `CurrentMatchStateTimeout` sent with the `BackendMatchCreation`(5) update (s506: 10s).
const MATCH_STATE_BACKEND_TIMEOUT: f32 = 10.0;

/// The retail round-0 `MatchState` progression AFTER `BackendMatchCreation`(5) —
/// the path the client walks from "Setting up…" into the live fight. Each entry is
/// `(state, hold_before_this_state, state_timeout_secs)`: `hold_before` is how long
/// to wait (since entering the *previous* state) before sending this state's op55
/// update; `state_timeout_secs` is the `CurrentMatchStateTimeout` value (Match
/// propId6) the client shows as the countdown for that state.
///
/// **Capture-proven against s506 obj 123 (round 0), 2026-06-19** — the decoded op55
/// (carrier 0x35) MatchState updates and their wall-clock deltas:
/// ```text
///  5 BackendMatchCreation    05:05:40  (10s)   ← entered by the FSM (Spawning→BackendMatchCreated)
///  6 OpponentFoundFeedback   05:05:40  (1.5s)  +0s  (same tick as 5)
///  7 PreMatch                05:05:42  (3.0s)  +2s
/// 11 OpponentShowcase        05:05:45  (12.0s) +3s  (round 0 SKIPS 8/9/10 — those are the
///                                                     between-rounds loadout re-choice, seen
///                                                     only in round 1)
/// 12 PreRound                05:05:57  (4.0s)  +12s (== the OpponentShowcase timeout)
/// 13 InRound                 05:06:02  (120s)  +5s  ← THE FIGHT (client enters the combat scene)
/// ```
/// Every transition is **server-timer-driven** (each inter-state gap ≈ the prior
/// state's `CurrentMatchStateTimeout`); none waits on a specific client message. The
/// client uploads its loadout EARLY (c2s op54 PlayerLoadoutReady/gmid20 + gmid36 during
/// states 3→5, before this sequence) and emits periodic op80 `MatchStateChangeAck`
/// flow echoes (handled as a no-op in `on_c2s`/`resolve`), so the server simply walks
/// the states on its own clock. Combat inputs (op37/op46) begin only after InRound(13).
/// The holds below are rounded from the s506 deltas (6:+0s, 7:+2s, 11:+3s, 12:+12s,
/// 13:+5s); the timeouts are the exact captured propId6 values.
const MATCH_STATE_ROUND0_PROGRESSION: &[(MatchState, Duration, f32)] = &[
    (MatchState::OpponentFoundFeedback, Duration::from_secs(0), 1.5),
    (MatchState::PreMatch, Duration::from_secs(2), 3.0),
    (MatchState::OpponentShowcase, Duration::from_secs(3), 12.0),
    (MatchState::PreRound, Duration::from_secs(12), 4.0),
    (MatchState::InRound, Duration::from_secs(5), 120.0),
];

/// The retail post-match `MatchState` walk AFTER a round-ending death — the path the
/// client follows from the kill to a clean result screen + lobby return. Each entry is
/// `(state, hold_before_this_state, state_timeout_secs)`, same shape as
/// [`MATCH_STATE_ROUND0_PROGRESSION`]: `hold_before` is the wait (since the PREVIOUS
/// state was entered) before this state's op55 update; `state_timeout_secs` is the
/// `CurrentMatchStateTimeout` (Match propId6) the client shows.
///
/// **Capture-proven against s506 obj 123 (the FINAL round of a best-of-3), 2026-06-19**
/// — decoded from prod `arena_udp_frames`. `PostRound`(14) is emitted by `resolve` at
/// the death itself (with op29 + op79 RoundEnd + op48 result); THIS table is the walk
/// the FSM does AFTER PostRound:
/// ```text
/// 14 PostRound             05:07:01  (3.0)         ← emitted at the death (resolve.rs)
///    op79 "StateTimeout"   05:07:04  (flow heartbeat, +3s after PostRound)
/// 17 BackendMatchEnd       05:07:05  (20.0)  +4s   (from PostRound)
/// 16 PostMatch             05:07:11  (5.0)   +6s
/// 19 DisconnectingPlayers… 05:07:16  (~0)    +5s   ← terminal → match Finished
/// ```
/// Notable: retail **skips Victory(15)** and emits **BackendMatchEnd(17) BEFORE
/// PostMatch(16)** (the enum order is not the wire order). The inter-state gaps are the
/// observed wall-clock deltas (4/6/5 s); the timeouts are the exact captured propId6
/// values. We send an op79 `StateTimeout` flow heartbeat between PostRound and
/// BackendMatchEnd to mirror the s506 +3s heartbeat (kept simple — the periodic
/// StateTimeout heartbeat is suppressed in RoundEnd, so this is the only one).
const MATCH_STATE_MATCHEND_PROGRESSION: &[(MatchState, Duration, f32)] = &[
    (MatchState::BackendMatchEnd, Duration::from_secs(4), 20.0),
    (MatchState::PostMatch, Duration::from_secs(6), 5.0),
    (MatchState::DisconnectingPlayersAfterMatch, Duration::from_secs(5), 0.0),
];

pub struct MatchInstance {
    combat: MatchCombat,
    /// s2c ENet reliable sequence (used by the raw-socket dev path framing).
    s2c_seq: u16,
    last_heartbeat: Instant,
    /// **DEBUG (`ARENA_DEBUG_HOLD`).** When set, the FSM still drives the FULL
    /// round-start burst (Connecting→Spawning→BackendMatchCreated, byte-identical
    /// to normal) but then HOLDS at `BackendMatchCreated` forever — it never
    /// transitions to `StateTimeout`, so no combat phase is entered and no bot
    /// swings (the live round never starts). Lets us hand-inject s2c frames into a
    /// solo-connected match and watch the client with an unlimited window. OFF
    /// (false) in all normal operation + tests → existing behavior is unchanged.
    debug_hold: bool,
    /// Cursor into [`MATCH_STATE_ROUND0_PROGRESSION`] while the FSM is in the
    /// `BackendMatchCreated` phase: the index of the NEXT round-0 MatchState to emit
    /// (`6→7→11→12→13`). Starts at 0 (OpponentFoundFeedback) when `BackendMatchCreated`
    /// is entered; once it reaches the end (InRound has been broadcast) the FSM enters
    /// `StateTimeout` (the live combat round). Reset implicitly per match (one
    /// `MatchInstance` per match).
    setup_step: usize,
}

impl MatchInstance {
    /// Create a match instance with `capacity` fighters, each built from the
    /// matching entry of `loadouts` (missing entries default — bot / un-imported).
    /// `expected_peers` is how many real ENet peers must connect before the round
    /// starts (== capacity for PvP; 1 for a solo-vs-bot match, whose 2nd fighter is
    /// a server-driven bot with no peer).
    pub fn new(capacity: usize, expected_peers: usize, loadouts: Vec<Loadout>, now: Instant) -> Self {
        let mut combat = MatchCombat::new(capacity, expected_peers, now);
        for slot in 0..capacity {
            let net_object_id = combat.alloc_net_object_id();
            let player_net_object_id = combat.alloc_net_object_id();
            let ability_net_object_id = combat.alloc_net_object_id();
            // Use the provided loadout if it carries a weapon; else a starter
            // loadout so the damage model produces a real, progressing fight.
            let loadout = loadouts
                .get(slot)
                .cloned()
                .filter(|l| !l.weapon.base_by_type.is_empty())
                .unwrap_or_else(super::loadout::starter);
            let mut fighter = Fighter::new(slot, net_object_id, loadout, now);
            fighter.player_net_object_id = player_net_object_id;
            fighter.ability_net_object_id = ability_net_object_id;
            combat.fighters.push(fighter);
        }
        // The single type-54 Match net-object id (its replicated propId5 = MatchState
        // drives player binding). Allocated after the fighters so the per-fighter id
        // range is unchanged.
        combat.match_net_object_id = combat.alloc_net_object_id();
        MatchInstance {
            combat,
            s2c_seq: 0,
            last_heartbeat: now,
            // Read the DEBUG-HOLD flag once at construction. Off (false) when the
            // env var is unset — i.e. in every test and all normal operation.
            debug_hold: super::debug_hold_enabled(),
            setup_step: 0,
        }
    }

    /// Set the match's `gameSessionId` (the Match net-object propId9). Called by the
    /// registry right after allocation. Defaults to empty (the binding gate is the
    /// MatchState at propId5, not the session id).
    pub fn set_game_session_id(&mut self, game_session_id: impl Into<String>) {
        self.combat.game_session_id = game_session_id.into();
    }

    pub fn next_seq(&mut self) -> u16 {
        let s = self.s2c_seq;
        self.s2c_seq = self.s2c_seq.wrapping_add(1);
        s
    }

    pub fn state_name(&self) -> &'static str {
        self.combat.phase_name()
    }

    /// True once the match has run its full course — the post-match MatchState walk
    /// reached the terminal `DisconnectingPlayersAfterMatch`(19) and the FSM finished.
    /// The registry uses this to actively ENet-disconnect the player(s) at match-end
    /// (the literal meaning of state 19), so the client leaves the result screen and
    /// returns to the arena lobby instead of holding the connection open.
    pub fn is_finished(&self) -> bool {
        matches!(self.combat.phase, FlowState::Finished)
    }

    /// The character display name of the fighter in `slot`, if any (empty for a
    /// starter/bot loadout). Used by the DEBUG peer listing to label a target.
    pub fn fighter_display_name(&self, slot: usize) -> &str {
        self.combat
            .fighters
            .get(slot)
            .map(|f| f.loadout.display_name.as_str())
            .unwrap_or("")
    }

    /// Drive the engine on a decrypted inbound c2s `user_data` (`marker ‖
    /// MessageType ‖ body`) from player `sender`.
    pub fn on_c2s(&mut self, sender: usize, user_data: &[u8], now: Instant) -> Vec<(usize, Vec<u8>)> {
        let mut out = Vec::new();
        debug!(
            "combat c2s: slot {sender} carrier 0x{:02x} ({} bytes) in phase {}",
            user_data.get(1).copied().unwrap_or(0),
            user_data.len(),
            self.combat.phase_name(),
        );

        // op58 CLOCK-SYNC — answered in ANY phase (the client sends this c2s on
        // connect and BLOCKS at MatchState `AwaitingClientBackendSynchronization`
        // until we reply, BEFORE it uploads its loadout — so it lands during
        // Connecting/Spawning/BackendMatchCreated, all phases `resolve` drops). Reply
        // to the SENDER ONLY: first Long = the server clock ticks (same .NET-ticks
        // source `broadcast_clock` uses), second Long = the client's token echoed
        // verbatim. The client's c2s op58 is two Longs `[clock0=0 @propId0, token
        // @propId1]`; echoing propId1 unblocks it. [capture-proven from s506:
        // client token EE3FEB9B2DCCDE08 → reply must echo EE3FEB9B2DCCDE08.]
        if user_data.get(1) == Some(&CARRIER_CLOCK) {
            // propId1 is the token (a `Long`); echo it verbatim. Fall back to 0 on a
            // malformed body so we still reply (never panic, never hang the client).
            let parsed = arena_proto::parse_netdata(&user_data[2..]);
            let token = match parsed.props.get(&1) {
                Some(NetDataValue::Long(v)) => *v,
                Some(other) => other.as_i64().unwrap_or(0),
                None => 0,
            };
            let server_clock = Self::clock_ticks();
            let reply = messages::clock(server_clock, token);
            // DIAGNOSTIC (op58 gate, candidate 1): the EXACT inbound op58 bytes, each
            // parsed NetData Long (propId→value), the reply we emit, and the channel
            // the enet host will route it on (small → 0). Diffed against retail s506
            // (server prop0 = own clock-ticks, prop1 = echoed token).
            info!(
                "ARENA-DIAG op58 c2s slot {sender}: raw={} | parsed props={} | reply={} | reply channel={}",
                hex_lower(user_data),
                fmt_netdata_longs(&parsed),
                hex_lower(&reply),
                if reply.len() > 1000 { 4 } else { 0 },
            );
            out.push((sender, reply));
            return out;
        }

        // op61 LoadoutClientBackendSynchronized (c2s) — the client reports its OWN
        // loadout-backend sync (+ a HideHelmet cosmetic flag) at a round transition.
        // Capture-proven CLIENT→SERVER only (s506 #3523229 + every retail match); the
        // server NEVER sends op61 and does NOT need to reply — the client self-advances
        // its PvpState/MatchState once it has the profile + spawns and emits op54
        // PlayerLoadoutReady. We simply ACK it into the void (no s2c) so it is never
        // mis-resolved as a combat swing. [docs/arena-journey-log.md §7]
        if messages::is_loadout_backend_synchronized(user_data) {
            debug!("combat c2s: slot {sender} op61 LoadoutClientBackendSynchronized — handshake, no reply");
            return out;
        }

        // ConcedeMatch ends the match for everyone. (Heuristic: the concede
        // carrier byte == GameMessageId::ConcedeMatch; refine if a capture shows
        // otherwise.)
        if user_data.get(1) == Some(&(GameMessageId::ConcedeMatch as u8))
            && !matches!(self.combat.phase, FlowState::Finished)
        {
            info!("combat: slot {sender} conceded → match Finished");
            self.combat.phase = FlowState::Finished;
            for slot in 0..self.combat.fighters.len() {
                if let Some(m) = messages::flow_state(self.combat.flow_controller_id, FlowState::RoundEnd) {
                    out.push((slot, m));
                }
            }
            return out;
        }

        // Everything else (swipe / ability / block / position) → resolution.
        let phase_before = self.combat.phase;
        out.extend(resolve::on_c2s_input(&mut self.combat, sender, user_data, now));
        // A killing blow flips StateTimeout→RoundEnd inside `resolve::end_match`
        // (emitting op29 + op79 RoundEnd + op48 + MatchState→PostRound). Anchor the
        // post-match MatchState walk to NOW so its s506 timers run from the death, not
        // from whenever StateTimeout was entered.
        if phase_before != FlowState::RoundEnd && self.combat.phase == FlowState::RoundEnd {
            self.combat.phase_entered = now;
        }
        out
    }

    /// Server-initiated messages for this tick. `connected` is the number of
    /// peers that have completed the handshake (from the match's player list).
    pub fn on_tick(&mut self, connected: usize, now: Instant) -> Vec<(usize, Vec<u8>)> {
        let mut out = Vec::new();
        match self.combat.phase {
            // Wait for everyone to connect, then create the match: announce
            // BackendMatchCreated + the combat screen for every avatar to everyone.
            FlowState::Connecting => {
                if self.combat.expected_peers() > 0 && connected >= self.combat.expected_peers() {
                    info!(
                        "combat FSM: Connecting → Spawning ({connected}/{} peer(s), {} fighter(s))",
                        self.combat.expected_peers(),
                        self.combat.capacity(),
                    );
                    self.combat.phase = FlowState::Spawning;
                    self.combat.phase_entered = now;
                    self.last_heartbeat = now;
                    // Spawn/profile burst — NO BackendMatchCreated yet, and NO op58
                    // clock: the s2c op58 is the REPLY to the client's c2s op58
                    // clock-sync (handled in `on_c2s`), NOT an unsolicited broadcast.
                    // Retail sends the spawns + profile first; the client uploads its
                    // loadout during the Spawning hold; BackendMatchCreated follows ~4s
                    // later (s506 stagger).
                    // Round-start, retail-faithful order (s506 obj-123 match): the two
                    // Player spawns + the Match net-object (st 3, pc 1) + the local
                    // PlayerWelcome go FIRST. The Avatars + the opponent PROFILE + stat
                    // words come LATER, right after the Match flips to InitialPlayerSetup(4)
                    // (see the Spawning branch's 3→4 block) — NOT in this burst. This
                    // ordering is what binds the players: both Player net-objects must be
                    // registered in PvpClientManager._pvpPlayers before the avatars'
                    // discovery callbacks look them up (GetPvpPlayer by char-UUID).
                    self.broadcast_spawns(&mut out);
                    // The Match net-object was spawned (in broadcast_spawns) carrying
                    // MatchState=WaitingForPlayers(3); record it so the FSM advances it
                    // 3→4→5 from here (the player-binding gate).
                    self.combat.match_state = MatchState::WaitingForPlayers;
                    self.broadcast_welcome(&mut out);
                    // Round-start emission audit — confirm what actually goes on the wire:
                    // carrier→count (58=clock, 50=spawn, 54=profile/flow) + each fighter's
                    // profile-JSON size (0 ⇒ empty ⇒ broadcast_profiles skipped it ⇒ the
                    // client can't build its opponent ⇒ "Connecting…" stall).
                    let mut carriers = std::collections::BTreeMap::new();
                    for (_, b) in &out {
                        if b.len() >= 2 {
                            *carriers.entry(b[1]).or_insert(0u32) += 1;
                        }
                    }
                    let profile_bytes: Vec<usize> = self
                        .combat
                        .fighters
                        .iter()
                        .map(|f| f.loadout.profile_character_json.len())
                        .collect();
                    info!(
                        "combat round-start emit: {} frames, carriers(dec) {:?}, profile_json_bytes {:?}",
                        out.len(),
                        carriers,
                        profile_bytes
                    );
                    // DIAGNOSTIC (op54 gate, candidate 2): for EVERY emitted frame, log
                    // (viewer, carrier, plaintext len, the enet channel it routes to:
                    // >1000 → ch4 else ch0). For the big op54 PROFILE (carrier 0x36,
                    // >1000 B) also estimate the rusty_enet fragment count at MTU 1392
                    // (frag_len ≈ 1372, matching retail's 1372 B/frag). Diff vs retail
                    // s506: profile = 16 frags on ch4; small frames on ch0/ch1.
                    const FRAG_LEN: usize = 1372; // rusty_enet HOST_DEFAULT_MTU 1392 − ENet header
                    for (viewer, b) in &out {
                        let carrier = b.get(1).copied().unwrap_or(0);
                        let channel = messages::retail_channel(b);
                        if b.len() > 1000 {
                            let frags = b.len().div_ceil(FRAG_LEN);
                            info!(
                                "ARENA-DIAG op54 PROFILE → viewer {viewer}: carrier 0x{carrier:02x}, \
                                 plaintext {} B → channel {channel}, ~{frags} fragments @ {FRAG_LEN} B/frag",
                                b.len()
                            );
                        } else {
                            debug!(
                                "ARENA-DIAG frame → viewer {viewer}: carrier 0x{carrier:02x}, {} B → channel {channel}",
                                b.len()
                            );
                        }
                    }
                }
            }
            // Hold (retail ~4s) so the client drives its loadout-upload handshake,
            // THEN announce BackendMatchCreated — never in the same tick as the spawns.
            FlowState::Spawning => {
                let elapsed = now.duration_since(self.combat.phase_entered);
                // ~1s after the spawn (which carried MatchState=WaitingForPlayers=3),
                // advance the Match net-object to InitialPlayerSetup(4). The client binds
                // its local/opponent PvpPlayer across states 3→4 (the HasLocalPlayer gate);
                // retail s506 obj 123: 3 @05:05:36 → 4 @05:05:37.
                if self.combat.match_state == MatchState::WaitingForPlayers
                    && elapsed >= MATCH_SETUP_STAGGER
                {
                    info!("combat FSM: Match state WaitingForPlayers(3) → InitialPlayerSetup(4)");
                    // Match → InitialPlayerSetup(4), PlayerCount→2 (broadcast_match_state
                    // passes fighters.len()). s506 obj 123: 3 @05:05:36 → 4 @05:05:37, pc 1→2.
                    self.broadcast_match_state(
                        &mut out,
                        MatchState::InitialPlayerSetup,
                        MATCH_STATE_SETUP_TIMEOUT,
                    );
                    // NOW (after state-4, retail order) the Avatars + the opponent PROFILE +
                    // stat words. Retail s506 sends own Avatar(#3522349) → opp PROFILE
                    // (#3522353) → opp Avatar(#3522368) → avatar stat words — all AFTER the
                    // Match reaches InitialPlayerSetup(4). Each Avatar's discovery binds its
                    // player (GetPvpPlayer by char-UUID → _{local,opponent}Info.Player), so
                    // both Players (sent in the WaitingForPlayers burst above) are already in
                    // _pvpPlayers by the time these resolve. broadcast_avatars sends BOTH the
                    // own (Autonomous) AND opponent (Simulated) avatars — the Simulated one is
                    // what flips HasOpponentPlayer (proven on-device 2026-06-19).
                    self.broadcast_avatars(&mut out);
                    self.broadcast_profiles(&mut out);
                    self.broadcast_stat_updates(&mut out);
                }
                if elapsed >= SPAWN_HANDSHAKE_HOLD {
                    info!("combat FSM: Spawning → BackendMatchCreated (round-start handshake settled)");
                    self.combat.phase = FlowState::BackendMatchCreated;
                    self.combat.phase_entered = now;
                    self.last_heartbeat = now;
                    // MatchState → BackendMatchCreation(5) (the Match net-object update),
                    // alongside the op79 stateName "BackendMatchCreated" on the flow
                    // controller. s506 obj 123: state 5 @05:05:40, same tick as the op79.
                    self.broadcast_match_state(
                        &mut out,
                        MatchState::BackendMatchCreation,
                        MATCH_STATE_BACKEND_TIMEOUT,
                    );
                    self.broadcast_flow(&mut out, FlowState::BackendMatchCreated);
                }
            }
            // Walk the Match net-object's MatchState through the retail round-0
            // progression — `BackendMatchCreation`(5) → `OpponentFoundFeedback`(6) →
            // `PreMatch`(7) → `OpponentShowcase`(11) → `PreRound`(12) → `InRound`(13) —
            // on per-state timers (s506 obj-123, capture-proven), then enter the live
            // combat round (`StateTimeout`). This is the LAST gate to the fight: the
            // client parks at "Setting up…" until the Match net-object's MatchState
            // moves past 5, and enters the combat scene when it reaches InRound(13).
            // Each step reuses the SAME `broadcast_match_state` mechanism that drove
            // 3→4→5 (op55 property update on obj 123). [MATCH_STATE_ROUND0_PROGRESSION]
            //
            // DEBUG-HOLD (`ARENA_DEBUG_HOLD`): stay at BackendMatchCreation(5) forever —
            // the FULL round-start burst has already gone out (Spawning transition),
            // but we never advance the MatchState past 5, so no combat phase is entered.
            // This is the freeze window for hand-injecting s2c frames.
            FlowState::BackendMatchCreated if self.debug_hold => {}
            FlowState::BackendMatchCreated => {
                // Emit the next round-0 MatchState once its `hold_before` has elapsed
                // since the previous state was entered (`phase_entered` tracks that).
                if let Some(&(state, hold_before, timeout)) =
                    MATCH_STATE_ROUND0_PROGRESSION.get(self.setup_step)
                {
                    if now.duration_since(self.combat.phase_entered) >= hold_before {
                        info!(
                            "combat FSM: MatchState → {:?}({}) [round-0 setup step {}/{}]",
                            state,
                            state as u8,
                            self.setup_step + 1,
                            MATCH_STATE_ROUND0_PROGRESSION.len(),
                        );
                        self.broadcast_match_state(&mut out, state, timeout);
                        self.setup_step += 1;
                        self.combat.phase_entered = now;
                    }
                } else {
                    // The whole progression has been emitted (last state = InRound(13));
                    // the client is now in the combat scene. Enter the live round.
                    info!("combat FSM: BackendMatchCreated → StateTimeout (InRound reached — round 1 live)");
                    self.combat.phase = FlowState::StateTimeout;
                    self.combat.phase_entered = now;
                    self.last_heartbeat = now;
                    self.combat.round = 1;
                    self.broadcast_flow(&mut out, FlowState::StateTimeout);
                }
            }
            // Round running: periodic StateTimeout heartbeat + combat resolution.
            FlowState::StateTimeout => {
                if now.duration_since(self.last_heartbeat) >= HEARTBEAT {
                    self.last_heartbeat = now;
                    self.broadcast_flow(&mut out, FlowState::StateTimeout);
                }
                let debug_hold = self.debug_hold;
                out.extend(resolve::on_tick(&mut self.combat, now, debug_hold));
                // A bot's killing blow on the tick flips StateTimeout→RoundEnd; anchor
                // the post-match walk to NOW (same as the on_c2s player-kill path).
                if self.combat.phase == FlowState::RoundEnd {
                    self.combat.phase_entered = now;
                }
            }
            // Post-match: a round-ending death just put the Match net-object at
            // PostRound(14) (resolve.rs emitted op29 + op79 RoundEnd + op48 result +
            // the PostRound update). Walk the terminal MatchState sequence
            // BackendMatchEnd(17)→PostMatch(16)→DisconnectingPlayers(19) on the s506
            // final-round timers, then finish the match — so the client shows a clean
            // result and returns to the lobby instead of timing out ("error 3").
            // [MATCH_STATE_MATCHEND_PROGRESSION]
            FlowState::RoundEnd => {
                if let Some(&(state, hold_before, timeout)) =
                    MATCH_STATE_MATCHEND_PROGRESSION.get(self.combat.matchend_step)
                {
                    if now.duration_since(self.combat.phase_entered) >= hold_before {
                        // Mirror the s506 +3s op79 "StateTimeout" flow heartbeat that
                        // rides between PostRound and BackendMatchEnd (only once, at the
                        // first terminal step).
                        if self.combat.matchend_step == 0 {
                            self.broadcast_flow(&mut out, FlowState::StateTimeout);
                        }
                        info!(
                            "combat FSM: post-match MatchState → {:?}({}) [matchend step {}/{}]",
                            state,
                            state as u8,
                            self.combat.matchend_step + 1,
                            MATCH_STATE_MATCHEND_PROGRESSION.len(),
                        );
                        self.broadcast_match_state(&mut out, state, timeout);
                        self.combat.matchend_step += 1;
                        self.combat.phase_entered = now;
                    }
                } else {
                    // The terminal state (DisconnectingPlayersAfterMatch=19) has been
                    // broadcast; the match is over.
                    info!("combat FSM: RoundEnd → Finished (post-match walk complete; client returns to lobby)");
                    self.combat.phase = FlowState::Finished;
                }
            }
            FlowState::NextState | FlowState::Finished => {}
        }
        out
    }

    /// The server match-clock value: `.NET DateTime.Ticks` (100 ns since year 1).
    /// The single tick source for op58 — used by the c2s clock-sync REPLY (the
    /// first Long) and by `broadcast_clock`, so both emit the same wall-clock ticks.
    fn clock_ticks() -> i64 {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        // .NET DateTime.Ticks = 100 ns since year 1; Unix epoch = 621355968000000000 ticks.
        621_355_968_000_000_000i64
            + (unix.as_secs() as i64) * 10_000_000
            + (unix.subsec_nanos() as i64) / 100
    }

    /// Broadcast the match CLOCK (op58) to every viewer. **No longer used in the
    /// round-start burst** — retail's s2c op58 is the REPLY to the client's c2s
    /// op58 clock-sync (see `on_c2s`), not an unsolicited broadcast. Retained for
    /// the builder/tests and any future server-pushed time sync. Two .NET-ticks
    /// Longs (server clock + match-start ref), both ≈ now. [docs §6.2, RE'd from s486.]
    fn broadcast_clock(&self, out: &mut Vec<(usize, Vec<u8>)>) {
        let ticks = Self::clock_ticks();
        for slot in 0..self.combat.fighters.len() {
            out.push((slot, messages::clock(ticks, ticks)));
        }
    }

    fn broadcast_flow(&self, out: &mut Vec<(usize, Vec<u8>)>, state: FlowState) {
        if let Some(msg) = messages::flow_state(self.combat.flow_controller_id, state) {
            for slot in 0..self.combat.fighters.len() {
                out.push((slot, msg.clone()));
            }
        }
    }

    /// Broadcast the round-start net-object SPAWNS (op50) to every viewer.
    ///
    /// **Object set is retail-faithful to s506 (proven by a field-by-field byte-diff,
    /// 2026-06-19):** the local client receives an op50 **Player** for BOTH fighters
    /// (role Autonomous for its OWN, Simulated for the opponent) but an op50 **Avatar**
    /// for the viewer's OWN (Autonomous) fighter ONLY. Retail NEVER sends an op50 Avatar
    /// for the Simulated opponent, and NEVER sends a type-54 ability/Match op50 at
    /// round-start (s506 emits exactly three op50: self-Player, opp-Player, self-Avatar).
    ///
    /// The opponent's avatar is a **client-local actor** the encounter builds from the
    /// op54 PROFILE (`PvpEncounter.SetupOpponentActor`/`OnOpponentLoadoutReceived`),
    /// NOT a network object. Spawning a *Simulated Avatar net-object* for the opponent
    /// made the client route that proxy into `PvpEncounter.SpawnOpponent(proxy)` (frida
    /// v1 confirmed it fired) but its addressables load never completed →
    /// `PvpEncounter.OnOpponentLoaded` NEVER fired, `OpponentPlayer`/`OpponentAvatar`
    /// stayed null, `ClientChecklist` never flipped → "Connecting…" forever (frida-proven:
    /// `OnPlayerResourceLoaded` fired ×2 but the opponent actor was never built).
    /// [docs/arena-journey-log.md §8; il2cpp PvpClientManager.OnObjectDiscover →
    /// PvpEncounter.SpawnOpponent → OnOpponentLoaded]
    ///
    /// The op54 PROFILE (gear/customization/stats JSON), broadcast right after, is what
    /// constructs the opponent — see `broadcast_profiles`.
    fn broadcast_spawns(&self, out: &mut Vec<(usize, Vec<u8>)>) {
        for viewer in 0..self.combat.fighters.len() {
            for actor in &self.combat.fighters {
                let is_own = actor.slot == viewer;
                let role = if is_own {
                    NetRole::Autonomous
                } else {
                    NetRole::Simulated
                };
                let name = if actor.loadout.display_name.is_empty() {
                    "Fighter"
                } else {
                    actor.loadout.display_name.as_str()
                };
                // Player op50 — for BOTH fighters (self Autonomous, opponent Simulated).
                // Retail s506 sends the two Player spawns FIRST (obj 120 role3, obj 122
                // role2), BEFORE either Avatar — the Avatars come later (see
                // `broadcast_avatars`, after the Match→InitialPlayerSetup transition).
                // Both Player net-objects must be registered in `PvpClientManager._pvpPlayers`
                // before the avatars' resource-load callbacks run, because the avatar
                // discovery is what BINDS the player to the encounter:
                // `PvpEncounter.FinishSpawnLocalAvatar`/`SpawnOpponent` →
                // `PvpClientManager.GetPvpPlayer(<avatar charUUID>)` → set
                // `_{local,opponent}Info.Player` (the `HasLocalPlayer`/`HasOpponentPlayer`
                // gate). [il2cpp RE 2026-06-19: GetPvpPlayer @0x1ADF51C matches a
                // registered PvpPlayer by the avatar's propId4 character UUID.]
                out.push((
                    viewer,
                    messages::spawn_player(
                        actor.player_net_object_id,
                        role,
                        name,
                        &actor.loadout.character_uuid,
                        actor.loadout.level as i32,
                        actor.loadout.level as i32,
                    ),
                ));
            }
            // op50 SPAWN of the single type-54 **Match** net object — the object whose
            // replicated propId5 = `MatchState` the client reads to advance the match.
            // Spawned with `WaitingForPlayers`(3) + **PlayerCount 1** (retail s506 obj 123
            // spawn: role 2 Simulated, st 3, pc 1, timeout 20s) — the FSM then flips it to
            // `InitialPlayerSetup`(4) with **PlayerCount 2** once both players are present
            // (`broadcast_match_state`), exactly as s506 obj 123 does at its 3→4 update.
            out.push((
                viewer,
                messages::spawn_match(
                    self.combat.match_net_object_id,
                    1, // retail s506: Match spawns at PlayerCount=1, flips to 2 at state 3→4
                    MatchState::WaitingForPlayers,
                    MATCH_STATE_WAIT_TIMEOUT,
                    self.combat.round,
                    &self.combat.game_session_id,
                ),
            ));
        }
    }

    /// Broadcast the round-start **Avatar** op50 spawns — both the viewer's OWN
    /// (Autonomous) and the OPPONENT's (Simulated) fighter body. Retail s506 sends
    /// BOTH (obj 124 role3 + obj 125 role2), AFTER the two Player spawns and AFTER the
    /// Match net-object reaches `InitialPlayerSetup`(4) (own avatar @ #3522349, opponent
    /// avatar @ #3522368, interleaved with the opponent profile). **Each avatar's
    /// discovery is the player-binding trigger** (`HasLocalPlayer`/`HasOpponentPlayer`):
    /// the client's `PvpEncounter.FinishSpawnLocalAvatar`/`SpawnOpponent` looks the
    /// avatar's character UUID (NetData propId4) up in `_pvpPlayers` via
    /// `PvpClientManager.GetPvpPlayer` and sets `_{local,opponent}Info.Player`. Without
    /// the OPPONENT (Simulated) avatar, `HasOpponentPlayer` never flips — proven
    /// on-device 2026-06-19: injecting the missing Simulated avatar flipped it 0→1.
    /// [il2cpp RE: Match.get_HasLocalPlayer @0x178AAF4 → _pvpEncounter._localInfo.Player.]
    fn broadcast_avatars(&self, out: &mut Vec<(usize, Vec<u8>)>) {
        for viewer in 0..self.combat.fighters.len() {
            for actor in &self.combat.fighters {
                let role = if actor.slot == viewer {
                    NetRole::Autonomous
                } else {
                    NetRole::Simulated
                };
                out.push((
                    viewer,
                    messages::spawn_avatar(actor.net_object_id, role, &actor.loadout.character_uuid),
                ));
            }
        }
    }

    /// Broadcast a type-54 Match net-object **property update** (op55) that advances
    /// the replicated `MatchState` (propId5) to `state`. The client's
    /// `Match.OnObjectPropertiesChanged` applies it and fires `OnMatchStateChanged`,
    /// binding the local/opponent `PvpPlayer` during `WaitingForPlayers`(3) /
    /// `InitialPlayerSetup`(4). Also records the state on `MatchCombat`. [s506 obj 123]
    fn broadcast_match_state(&mut self, out: &mut Vec<(usize, Vec<u8>)>, state: MatchState, timeout_secs: f32) {
        self.combat.match_state = state;
        for viewer in 0..self.combat.fighters.len() {
            out.push((
                viewer,
                messages::update_match(
                    self.combat.match_net_object_id,
                    self.combat.fighters.len() as u8,
                    state,
                    timeout_secs,
                    self.combat.round,
                    &self.combat.game_session_id,
                ),
            ));
        }
    }

    /// op21 `PlayerWelcome` to each viewer's OWN Player object — the FIRST
    /// carrier-0x36 user-message of the round-start (retail s506). The client's
    /// `PvpPlayer` needs this to enter the user-message / loadout-upload phase;
    /// without it `NetObjectModule.OnUserMessage` never fires and the client hangs
    /// at "Connecting…" after ACKing the spawns. Sent ONLY to the viewer about its
    /// own (Authority) player object. [diffed live 2026-06-19 vs s506: the missing message.]
    fn broadcast_welcome(&self, out: &mut Vec<(usize, Vec<u8>)>) {
        for viewer in 0..self.combat.fighters.len() {
            let player_obj = self.combat.fighters[viewer].player_net_object_id;
            // p4: a small per-player arena-state byte (s506 observed 20/21, semantics
            // unconfirmed and not the gate). Use the documented default constant.
            out.push((viewer, messages::player_welcome(player_obj, 20)));
        }
    }

    /// op54-small per-avatar stat/HP word (full at round-start) — finalizes each
    /// actor's health on the client. [s486 round-start]
    fn broadcast_stat_updates(&self, out: &mut Vec<(usize, Vec<u8>)>) {
        for viewer in 0..self.combat.fighters.len() {
            for actor in &self.combat.fighters {
                out.push((viewer, messages::stat_update(actor.net_object_id)));
            }
        }
    }

    /// Broadcast the op54 PROFILE (full character + equipped-gear JSON) a client needs to
    /// construct the OPPONENT's avatar — appearance/gear/abilities/PvP stats
    /// (`SetupOpponentActor`/`LoadoutJSON`). Large (tens of KB) → rusty_enet fragments it
    /// on ENet channel 4. Skipped for fighters with no profile (starter loadout / bot).
    /// Sent after the op50 spawns, before the flow states (docs/arena-protocol-spec.md §6.2).
    ///
    /// **Opponent-only — each viewer gets ONLY its opponent's profile, never its own.**
    /// The retail server never echoes a client its own profile during setup: the client
    /// already has it (it uploads its own via op54 *c2s*); the server relays only the
    /// *other* player's. Verified from s506 (video↔capture): the client receives exactly
    /// one op54 profile = the opponent's (`05:05:38`). Sending a client a profile for its
    /// OWN (Autonomous) object — an Authority-role op54 it never expects, emitted first —
    /// stalled the client's profile pipeline so the opponent's profile (sent right after)
    /// was never applied → the match sat at "Connecting…", never "Setting up…" (the
    /// 2026-06-17 paired-match stall). [docs/arena-journey-log.md §7]
    fn broadcast_profiles(&self, out: &mut Vec<(usize, Vec<u8>)>) {
        for viewer in 0..self.combat.fighters.len() {
            for actor in &self.combat.fighters {
                if actor.slot == viewer {
                    continue; // never send a client its OWN profile (retail: opponent-only)
                }
                if actor.loadout.profile_character_json.is_empty() {
                    continue;
                }
                out.push((
                    viewer,
                    messages::player_profile(
                        actor.player_net_object_id,
                        &actor.loadout.profile_equipped_json,
                        &actor.loadout.profile_character_json,
                    ),
                ));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn phase(&self) -> FlowState {
        self.combat.phase
    }

    /// Test-only: the current replicated `MatchState` on the Match net object.
    #[cfg(test)]
    pub(crate) fn match_state_for_test(&self) -> MatchState {
        self.combat.match_state
    }

    /// Test-only: force the DEBUG-HOLD flag without touching the process env (which
    /// no test mutates). Mirrors `ARENA_DEBUG_HOLD` being set at construction.
    #[cfg(test)]
    pub(crate) fn set_debug_hold(&mut self, hold: bool) {
        self.debug_hold = hold;
    }

    #[cfg(test)]
    pub(crate) fn fighter_health(&self, slot: usize) -> u32 {
        self.combat.fighters[slot].health
    }

    #[cfg(test)]
    pub(crate) fn fighter_max_health(&self, slot: usize) -> u32 {
        self.combat.fighters[slot].max_health
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(capacity: usize) -> (MatchInstance, Instant) {
        let now = Instant::now();
        // PvP-style: every fighter has a real peer (expected_peers == capacity).
        (MatchInstance::new(capacity, capacity, vec![], now), now)
    }

    /// Total wall-clock the round-start FSM needs to reach the LIVE round from t0:
    /// the spawn-handshake hold + the full round-0 MatchState walk (5→6→7→11→12→13)
    /// + one tick to enter StateTimeout. Computed from the constants so it tracks any
    /// retuning of the progression. (s506: 4s + (0+2+3+12+5)s ≈ 26s.)
    fn setup_to_live() -> Duration {
        let walk: Duration = MATCH_STATE_ROUND0_PROGRESSION
            .iter()
            .map(|(_, hold, _)| *hold)
            .sum();
        SPAWN_HANDSHAKE_HOLD + walk
    }

    /// Drive an existing match `m` from t0 to the LIVE round (StateTimeout) by ticking
    /// at 100 ms across the whole setup window (spawn-handshake hold + the MatchState
    /// walk). `connected` is the peer count to report each tick. Stops on the tick that
    /// first enters StateTimeout and returns THAT instant (`last_heartbeat` is set then)
    /// so callers can continue from the live moment without ticking into the past. One
    /// tick per 100 ms is ≫ enough for the FSM (smallest hold 0s, largest 12s).
    fn drive_to_live(m: &mut MatchInstance, connected: usize, t0: Instant) -> Instant {
        let step = Duration::from_millis(100);
        let total = setup_to_live() + Duration::from_secs(2);
        let n = (total.as_millis() / step.as_millis()) as u32;
        for i in 0..=n {
            let now = t0 + step * i;
            m.on_tick(connected, now);
            if m.phase() == FlowState::StateTimeout {
                return now;
            }
        }
        panic!("drive_to_live did not reach the live round within {total:?}");
    }

    /// A match driven to the LIVE round (StateTimeout): connect → Spawning (spawn
    /// burst) → BackendMatchCreation(5) (after the stagger hold) → the round-0
    /// MatchState walk 6→7→11→12→13 → StateTimeout. Returns the engine, t0, and the
    /// instant the round went live (so heartbeat/combat ticks advance from there).
    fn live_inst(capacity: usize) -> (MatchInstance, Instant) {
        let (m, t0, _live) = live_inst_at(capacity);
        (m, t0)
    }

    /// Like [`live_inst`] but also returns the instant the round went live.
    fn live_inst_at(capacity: usize) -> (MatchInstance, Instant, Instant) {
        let now = Instant::now();
        let mut m = MatchInstance::new(capacity, capacity, vec![], now);
        let live = drive_to_live(&mut m, capacity, now);
        (m, now, live)
    }

    #[test]
    fn match_starts_when_all_connected() {
        let (mut m, t0) = inst(2);
        // Only one connected → still Connecting, no output.
        assert!(m.on_tick(1, t0).is_empty());
        assert_eq!(m.phase(), FlowState::Connecting);

        // Both connected → BackendMatchCreated + combat-screen for both avatars,
        // delivered to both players (slots 0 and 1).
        // Both connected → the spawn burst goes out and we enter Spawning; the
        // BackendMatchCreated flow is STAGGERED ~4s later (retail), NOT in this burst.
        let out = m.on_tick(2, t0);
        assert_eq!(m.phase(), FlowState::Spawning);
        assert_eq!(
            out.iter().filter(|(_, b)| b.ends_with(b"BackendMatchCreated")).count(),
            0,
            "BackendMatchCreated must NOT ride the spawn burst (retail staggers it)"
        );
        // Retail-faithful round-start order (s506 obj-123 match): the FIRST burst
        // (Connecting→Spawning) sends per viewer = a Player op50 for BOTH fighters + the
        // single type-54 **Match** net-object op50 (propId5 = WaitingForPlayers, pc 1).
        // The Avatars come LATER (after the Match→InitialPlayerSetup transition, see
        // `match_state_progresses_3_4_5`). So this burst = 2 viewers × (2 Player + 1
        // Match) = 6 op50 (0x32) spawn messages — NO Avatars yet.
        let spawns = out.iter().filter(|(_, b)| b.len() >= 2 && b[1] == 0x32).count();
        assert_eq!(spawns, 6, "op50 = Player(both) + Match per viewer; avatars come after state 3→4");
        // No Avatar (type 56) op50 in the first burst (they ride the 3→4 tick).
        let avatars = out
            .iter()
            .filter(|(_, b)| b.len() >= 3 && b[1] == 0x32 && arena_proto::parse_netdata(&b[2..]).int(1) == Some(56))
            .count();
        assert_eq!(avatars, 0, "Avatars are NOT in the WaitingForPlayers spawn burst");
        // The Match spawn carries MatchState=WaitingForPlayers(3) — NOT 5 — so the
        // client enters player binding instead of jumping straight to BackendMatchCreation.
        assert_eq!(
            m.match_state_for_test(),
            crate::arena::combat::state::MatchState::WaitingForPlayers,
            "Match net-object spawns at WaitingForPlayers(3), the binding-gate state"
        );
    }

    /// Reproduction guard (s506 decode): the round-start is STAGGERED — the spawn /
    /// profile burst goes out first, and BackendMatchCreated is announced only ~4s
    /// LATER, never in the same tick as the spawns. Batching them preempts the client's
    /// loadout-upload (PlayerLoadoutReady) handshake → hang at "Connecting".
    #[test]
    fn backend_match_created_is_staggered_after_spawns() {
        let (mut m, t0) = inst(2);
        let burst = m.on_tick(2, t0); // → Spawning
        assert_eq!(m.phase(), FlowState::Spawning);
        assert!(burst.iter().any(|(_, b)| b.len() >= 2 && b[1] == 0x32), "spawns in the first burst");
        assert!(
            !burst.iter().any(|(_, b)| b.ends_with(b"BackendMatchCreated")),
            "BackendMatchCreated must NOT ride the spawn burst"
        );
        // Held right up to the stagger deadline.
        let held = m.on_tick(2, t0 + SPAWN_HANDSHAKE_HOLD - Duration::from_millis(1));
        assert!(held.iter().all(|(_, b)| !b.ends_with(b"BackendMatchCreated")));
        assert_eq!(m.phase(), FlowState::Spawning);
        // After the hold: BackendMatchCreated to both players.
        let created = m.on_tick(2, t0 + SPAWN_HANDSHAKE_HOLD);
        assert_eq!(m.phase(), FlowState::BackendMatchCreated);
        assert_eq!(
            created.iter().filter(|(_, b)| b.ends_with(b"BackendMatchCreated")).count(),
            2,
            "BackendMatchCreated announced ~4s after the spawns, to both players"
        );
    }

    /// The Match net-object's replicated `MatchState` (propId5) progresses
    /// WaitingForPlayers(3) → InitialPlayerSetup(4) → BackendMatchCreation(5),
    /// mirroring retail s506 obj 123 — instead of the old jump straight to 5. This is
    /// the player-binding gate: the client binds its local/opponent `PvpPlayer` across
    /// states 3/4, so `HasLocalPlayer` only flips when 3 and 4 are seen.
    #[test]
    fn match_state_progresses_3_4_5() {
        use crate::arena::combat::state::MatchState;
        let (mut m, t0) = inst(2);
        // Spawn burst → MatchState spawned at WaitingForPlayers(3).
        m.on_tick(2, t0);
        assert_eq!(m.match_state_for_test(), MatchState::WaitingForPlayers);
        // ~1s later → InitialPlayerSetup(4), delivered as an op55 (0x35) update to both.
        let setup = m.on_tick(2, t0 + MATCH_SETUP_STAGGER);
        assert_eq!(m.match_state_for_test(), MatchState::InitialPlayerSetup);
        let setup_updates = setup
            .iter()
            .filter(|(_, b)| b.len() >= 2 && b[1] == 0x35)
            .count();
        assert_eq!(setup_updates, 2, "InitialPlayerSetup(4) → op55 update to both viewers");
        // After the stagger hold → BackendMatchCreation(5), again an op55 update.
        let created = m.on_tick(2, t0 + SPAWN_HANDSHAKE_HOLD);
        assert_eq!(m.match_state_for_test(), MatchState::BackendMatchCreation);
        assert!(
            created.iter().any(|(_, b)| b.len() >= 2 && b[1] == 0x35),
            "BackendMatchCreation(5) → op55 Match-object update"
        );
        // …and the op79 stateName "BackendMatchCreated" rides the SAME tick (flow controller).
        assert_eq!(
            created.iter().filter(|(_, b)| b.ends_with(b"BackendMatchCreated")).count(),
            2,
        );
    }

    /// Round-start sends each viewer ONLY the opponent's op54 profile, never its own —
    /// retail-faithful (s506). Echoing a client its own profile stalled "Setting up…".
    #[test]
    fn round_start_profile_is_opponent_only() {
        let now = Instant::now();
        // Two fighters that each carry a (non-empty) profile, so broadcast_profiles emits.
        let mk = |name: &str| {
            let mut l = crate::arena::combat::loadout::starter();
            l.display_name = name.to_string();
            l.profile_equipped_json = r#"{"equippedItems":{}}"#.to_string();
            l.profile_character_json = format!(r#"{{"name":"{name}"}}"#);
            l
        };
        let mut m = MatchInstance::new(2, 2, vec![mk("Alice"), mk("Bob")], now);
        m.on_tick(2, now); // Connecting → Spawning (players + Match spawn; NO profile yet)
        // The profiles ride the Match WaitingForPlayers(3)→InitialPlayerSetup(4) tick
        // (retail order: opponent PROFILE + avatars after state-4), not the first burst.
        let out = m.on_tick(2, now + MATCH_SETUP_STAGGER);
        // op54 PROFILE = carrier 0x36 with NetData propId3 == 35 (the profile gameMessageId);
        // distinct from op54-small stat word (p3=65) and flow states (p3=0x4F).
        let profiles: Vec<&(usize, Vec<u8>)> = out
            .iter()
            .filter(|(_, b)| b.len() > 2 && b[1] == 0x36 && arena_proto::parse_netdata(&b[2..]).int(3) == Some(35))
            .collect();
        assert_eq!(profiles.len(), 2, "exactly one op54 profile per viewer (opponent-only, not self)");
        // viewer 0 must receive slot 1's (Bob's) profile object id, NOT its own (slot 0).
        let p0 = profiles.iter().find(|(v, _)| *v == 0).expect("viewer 0 profile");
        assert_eq!(
            arena_proto::parse_netdata(&p0.1[2..]).int(0),
            Some(m.combat.fighters[1].player_net_object_id as i64),
            "viewer 0 receives the OPPONENT's (slot 1) profile, not its own"
        );
    }

    /// DEBUG-GHOST contract (docs/arena-ghost-gap-analysis.md). A **solo-vs-bot**
    /// match (capacity 2, expected_peers 1 — exactly the matchmaker's solo-fallback
    /// shape) emits the OPPONENT's op54 PROFILE (GameMessageId 35) to the lone human
    /// viewer (slot 0) IF AND ONLY IF the 2nd fighter (the bot, slot 1) has a
    /// NON-EMPTY `profile_character_json`:
    ///   - empty slot-1 loadout (today's `starter()` bot)  → ZERO opponent profiles
    ///     (the bug: `broadcast_profiles`' `is_empty()` guard skips it → the client's
    ///     `OpponentLoadoutReady` never flips → "Connecting…" forever);
    ///   - ghost slot-1 loadout (a real character's profile) → exactly ONE opponent
    ///     profile, addressed to slot 1's player object (the fix: `ARENA_DEBUG_GHOST`
    ///     loads a real char into `loadouts[1]` so this fires).
    /// The op54 PROFILE = carrier 0x36 with NetData propId3 == 35 (vs the op54-small
    /// stat word propId3==65 / the flow-state propId3==0x4F).
    #[test]
    fn solo_fallback_ghost_yields_broadcastable_opponent_profile() {
        let now = Instant::now();
        let is_profile = |b: &[u8]| {
            b.len() > 2 && b[1] == 0x36 && arena_proto::parse_netdata(&b[2..]).int(3) == Some(35)
        };

        // The opponent PROFILE rides the Match WaitingForPlayers(3)→InitialPlayerSetup(4)
        // tick (retail order), not the first Connecting→Spawning burst. So we tick once to
        // enter Spawning, then again at `+MATCH_SETUP_STAGGER` to drive the 3→4 emit.
        // (a) the BUG: solo-fallback with an EMPTY slot-1 (starter) bot. capacity 2 /
        // expected_peers 1 → one human peer is enough to start. No profile goes out.
        let mut buggy = MatchInstance::new(2, 1, vec![], now);
        buggy.on_tick(1, now); // Connecting → Spawning
        let setup = buggy.on_tick(1, now + MATCH_SETUP_STAGGER); // 3→4 (avatars + profiles)
        assert_eq!(
            setup.iter().filter(|(_, b)| is_profile(b)).count(),
            0,
            "empty starter bot → NO opponent profile (reproduces the 'Connecting…' stall)"
        );

        // (b) the FIX: feed slot 1 a real (non-empty-profile) ghost loadout — what
        // `ARENA_DEBUG_GHOST` makes `load_loadout(ghost)` produce. Slot 0 (the human)
        // stays an empty starter; the opponent profile must still broadcast.
        let mut ghost = crate::arena::combat::loadout::starter();
        ghost.display_name = "WolfWalker".into();
        ghost.profile_equipped_json = r#"{"equippedItems":{}}"#.into();
        ghost.profile_character_json = r#"{"name":"WolfWalker","level":89}"#.into();
        let mut fixed = MatchInstance::new(
            2,
            1,
            vec![crate::arena::combat::loadout::starter(), ghost],
            now,
        );
        fixed.on_tick(1, now); // → Spawning
        assert_eq!(fixed.phase(), FlowState::Spawning);
        let setup = fixed.on_tick(1, now + MATCH_SETUP_STAGGER); // 3→4 emit
        let profiles: Vec<&(usize, Vec<u8>)> =
            setup.iter().filter(|(_, b)| is_profile(b)).collect();
        assert_eq!(
            profiles.len(),
            1,
            "ghost slot-1 loadout → exactly ONE op54 opponent profile to the lone viewer"
        );
        // It is addressed to the lone human viewer (slot 0) and carries the OPPONENT's
        // (slot 1) player object id — never the viewer's own object.
        let (viewer, body) = profiles[0];
        assert_eq!(*viewer, 0, "the profile is delivered to the human viewer (slot 0)");
        assert_eq!(
            arena_proto::parse_netdata(&body[2..]).int(0),
            Some(fixed.combat.fighters[1].player_net_object_id as i64),
            "the profile addresses the OPPONENT (slot 1) player object"
        );
    }

    #[test]
    fn solo_bot_match_starts_on_one_peer_and_bot_attacks() {
        // capacity 2 (player + bot), but only 1 real peer expected → the match must
        // start when that lone peer connects (the bot has no peer), and the bot must
        // auto-swing at the player on the tick (a fight, not a static dummy).
        let now = Instant::now();
        let mut m = MatchInstance::new(2, 1, vec![], now);
        m.on_tick(1, now); // one real peer is enough → Spawning
        assert_eq!(m.phase(), FlowState::Spawning);
        drive_to_live(&mut m, 1, now); // walk the MatchState progression → live round
        assert_eq!(m.phase(), FlowState::StateTimeout);
        let before = m.fighter_health(0);
        // Past the bot's swing cadence → the bot (slot 1) damages the player (slot 0).
        m.on_tick(1, now + setup_to_live() + Duration::from_secs(3));
        assert!(m.fighter_health(0) < before, "bot should damage the player on tick");
    }

    #[test]
    fn advances_to_round_after_hold() {
        // (a) Mid-progression: at +6s into BackendMatchCreation (the MatchState walk
        // takes ~22s) the match is still NOT live — it's walking 6→7→11→12→13.
        let (mut mid, t0) = inst(2);
        mid.on_tick(2, t0); // → Spawning (spawn burst)
        assert_eq!(mid.phase(), FlowState::Spawning);
        let step = Duration::from_millis(100);
        for i in 0..=((SPAWN_HANDSHAKE_HOLD + Duration::from_secs(6)).as_millis() / 100) as u32 {
            mid.on_tick(2, t0 + step * i);
        }
        assert_eq!(mid.phase(), FlowState::BackendMatchCreated, "still walking MatchState, not live yet");

        // (b) The full drive reaches the live round (StateTimeout), round 1, and emits
        // a StateTimeout heartbeat once live.
        let (mut m, _t0b, live) = live_inst_at(2);
        assert_eq!(m.phase(), FlowState::StateTimeout);
        assert_eq!(m.combat.round, 1);
        let out = m.on_tick(2, live + HEARTBEAT);
        assert!(out.iter().any(|(_, b)| b.ends_with(b"StateTimeout")));
    }

    #[test]
    fn heartbeats_on_cadence() {
        let (mut m, _t0, live) = live_inst_at(2); // → StateTimeout (live round)
        // After the cadence elapses (measured from when it actually went live): a
        // StateTimeout heartbeat to both players.
        let out = m.on_tick(2, live + HEARTBEAT);
        assert_eq!(out.iter().filter(|(_, b)| b.ends_with(b"StateTimeout")).count(), 2);
    }

    /// Capture-proven (s506): the round-start op58 clock-sync is a CLIENT-INITIATED
    /// request/reply. The client sends c2s op58 `[clock0=0, token]` and BLOCKS at
    /// `AwaitingClientBackendSynchronization` until the server replies op58
    /// `[server_clock_ticks, token]` — echoing the SAME token back to that one
    /// client. `on_c2s` must answer it in the round-start phase (Spawning here), to
    /// the SENDER ONLY, echoing the token verbatim — NOT route it to `resolve`
    /// (which drops everything off StateTimeout) and NOT broadcast.
    #[test]
    fn op58_clock_sync_echoes_client_token() {
        const TOKEN: i64 = 0x08DECC2E11DD1E98u64 as i64; // s506's 2nd player token (981EDD11…)
        let now = Instant::now();
        let mut m = MatchInstance::new(2, 2, vec![], now);
        m.on_tick(2, now); // Connecting → Spawning (round-start burst)
        assert_eq!(m.phase(), FlowState::Spawning, "test drives to the round-start phase");

        // Build the client's c2s op58: two Longs [clock0=0 @propId0, token @propId1].
        // `messages::clock` writes exactly that NetData; patch the marker to the c2s
        // marker 0x84 so the frame mirrors a real client send (byte 0 is not parsed).
        let mut c2s = messages::clock(0, TOKEN);
        c2s[0] = 0x84;
        assert_eq!(c2s[1], 0x3a, "carrier is op58 (0x3a)");

        let out = m.on_c2s(0, &c2s, now);

        // Exactly one reply, to the SENDER (slot 0), carrier op58.
        assert_eq!(out.len(), 1, "op58 clock-sync replies to the sender ONLY (not a broadcast)");
        let (target, reply) = &out[0];
        assert_eq!(*target, 0, "reply goes back to the sender");
        assert_eq!(reply[0], messages::MARKER_S2C, "s2c marker 0xBE");
        assert_eq!(reply[1], 0x3a, "reply carrier is op58 (0x3a)");

        // The reply echoes the client's token verbatim at propId 1, and carries a
        // real server clock at propId 0 (a plausible .NET DateTime.Ticks, not 0).
        let nd = arena_proto::parse_netdata(&reply[2..]);
        assert_eq!(
            nd.props.get(&1),
            Some(&arena_proto::NetDataValue::Long(TOKEN)),
            "propId 1 Long == the client's token, echoed verbatim",
        );
        match nd.props.get(&0) {
            Some(&arena_proto::NetDataValue::Long(ticks)) => {
                assert!(ticks > 621_355_968_000_000_000, "propId 0 is a real .NET-ticks server clock");
            }
            other => panic!("propId 0 must be a Long server clock, got {other:?}"),
        }

        // Combat resolution did NOT run (no phantom damage from the handshake frame).
        assert_eq!(m.fighter_health(1), m.fighter_max_health(1), "no damage from a clock-sync");
    }

    /// A malformed c2s op58 (no parseable token) still gets a reply (token 0) — the
    /// server must never panic or silently drop it (that would re-hang the client).
    #[test]
    fn op58_clock_sync_malformed_replies_token_zero() {
        let now = Instant::now();
        let mut m = MatchInstance::new(2, 2, vec![], now);
        m.on_tick(2, now); // → Spawning
        // Carrier 0x3a but a truncated/empty NetData body → no propId 1.
        let out = m.on_c2s(0, &[0x84, 0x3a], now);
        assert_eq!(out.len(), 1, "still exactly one reply to the sender");
        let nd = arena_proto::parse_netdata(&out[0].1[2..]);
        assert_eq!(
            nd.props.get(&1),
            Some(&arena_proto::NetDataValue::Long(0)),
            "malformed token → echo 0 (graceful, never panic)",
        );
    }

    /// Capture-proven (s506 #3523229): the client sends op61
    /// `LoadoutClientBackendSynchronized` c2s at a round transition — even while the
    /// match is LIVE (StateTimeout). The server must treat it as a handshake signal:
    /// NO s2c reply and NO phantom damage (it rides carrier 0x36, the combat-input
    /// carrier, so the pre-fix catch-all `resolve_swing` would have hit the opponent).
    #[test]
    fn op61_loadout_backend_sync_is_handshake_not_a_swing() {
        let (mut m, t0) = live_inst(2); // LIVE round (StateTimeout) — combat resolves here
        let full = m.fighter_health(1);

        // Build the real s506 op61 (Player obj, role Autonomous, HideHelmet), c2s marker.
        let mut op61 = messages::loadout_client_backend_synchronized(
            m.combat.fighters[0].player_net_object_id,
            NetRole::Autonomous,
            true,
        );
        op61[0] = 0x84; // c2s marker (byte 0 is not parsed)

        let out = m.on_c2s(0, &op61, t0);
        assert!(out.is_empty(), "op61 produces NO s2c (the server never replies to it)");
        assert_eq!(m.fighter_health(1), full, "op61 must NOT damage the opponent (it's not a swing)");
        assert_eq!(m.phase(), FlowState::StateTimeout, "op61 does not change the match phase");
    }

    /// The other carrier-0x36 round-transition handshake frames (op36 PlayerLoadoutReady,
    /// op80 MatchStateChangeAck) likewise never resolve as combat in the live round.
    #[test]
    fn round_transition_handshake_frames_do_no_damage() {
        let (mut m, t0) = live_inst(2);
        let full = m.fighter_health(1);

        // op36 PlayerLoadoutReady (c2s).
        let mut op36 = {
            let mut w = arena_proto::NetDataWriter::new();
            w.int(0, m.combat.fighters[0].player_net_object_id).byte(1, 55).byte(2, 3).byte(3, 36);
            messages::frame_for_test(w.finish())
        };
        op36[0] = 0x84;
        assert!(m.on_c2s(0, &op36, t0).is_empty(), "op36 PlayerLoadoutReady → no s2c");

        // op80 MatchStateChangeAck (c2s echo).
        let mut op80 = messages::match_state_change_ack(m.combat.flow_controller_id, "StateTimeout");
        op80[0] = 0x84;
        assert!(m.on_c2s(0, &op80, t0).is_empty(), "op80 MatchStateChangeAck → no s2c");

        assert_eq!(m.fighter_health(1), full, "no handshake frame damages the opponent");
    }

    #[test]
    fn combat_input_damages_opponent() {
        let (mut m, t0) = live_inst(2); // → live round (StateTimeout); combat resolves

        // A (slot 0) sends a combat-input (carrier 54) → B (slot 1) takes damage;
        // a ReceiveDamage goes to both target and attacker.
        let out = m.on_c2s(0, &[0x84, 0x36], t0);
        assert_eq!(out.len(), 2, "ReceiveDamage to both target and attacker");
        for (_, ud) in &out {
            assert_eq!(ud[1], 0x36, "carrier 54");
            assert_eq!(
                arena_proto::parse_netdata(&ud[2..]).int(3),
                Some(50),
                "real GameMessageId 50 at propId 3"
            );
        }
        // B's HP pool dropped by the provisional swing (1023 - 80 = 943).
        // B's RAW HP dropped by the model swing. Starter = L30 Heavy weapon (Glass
        // base 120 + Remarkable +9 = 129) × Heavy crit 1.987 = 256.3 Slashing, + Shock
        // enchant (tier 2 → 60 × 1.987 = 119.2); health total 375.5 → 376 (the equal
        // Magicka drain is excluded). HP is raw (×3 arena pool); wire is a fraction.
        assert_eq!(m.fighter_max_health(1) - m.fighter_health(1), 376, "B raw HP −376");
        if let Some(arena_proto::NetDataValue::ULong(v)) =
            arena_proto::parse_netdata(&out[0].1[2..]).props.get(&4)
        {
            // Health is the low 10 bits of the HIGH 32 (stat word); seq is the low 32.
            assert!(((v >> 32) & 0x3ff) < 1023, "wire health is a fraction below full");
        }

        // A second swing within the cooldown is throttled (no double-hit).
        assert!(m.on_c2s(0, &[0x84, 0x36], t0).is_empty(), "throttled within cooldown");
    }

    #[test]
    fn fight_to_death_ends_match() {
        let (mut m, t0) = live_inst(2); // → live round

        // A keeps swinging (past the cooldown each time) until B dies. The killing
        // blow emits the capture-faithful round-end burst: op29 PlayerDead (carrier
        // 0x36, GMID 29), op79 "RoundEnd", op48 MatchPostRoundInfoMsg (the result),
        // and the Match net-object → PostRound(14). (Retail s506 sends op48, NOT op49.)
        let mut t = t0;
        let mut death_out = Vec::new();
        for _ in 0..20 {
            t += Duration::from_millis(500);
            let out = m.on_c2s(0, &[0x84, 0x36], t);
            let is_op29 = |b: &[u8]| {
                b.len() > 3 && b[1] == 0x36 && arena_proto::parse_netdata(&b[2..]).int(3) == Some(29)
            };
            if out.iter().any(|(_, b)| is_op29(b)) {
                death_out = out;
                break;
            }
        }
        assert!(!death_out.is_empty(), "A eventually kills B → op29 PlayerDead to all");
        // The death burst carries op79 "RoundEnd", op48 result (GMID 48), and the
        // PostRound(14) Match update (carrier 0x35, propId5 == 14).
        assert!(
            death_out.iter().any(|(_, b)| b.ends_with(b"RoundEnd")),
            "op79 RoundEnd flow on the killing blow"
        );
        assert!(
            death_out.iter().any(|(_, b)| b.len() > 3 && b[1] == 0x36
                && arena_proto::parse_netdata(&b[2..]).int(3) == Some(48)),
            "op48 MatchPostRoundInfoMsg (the result) on the killing blow"
        );
        assert!(
            death_out.iter().any(|(_, b)| b.len() > 2 && b[1] == 0x35
                && arena_proto::parse_netdata(&b[2..]).int(5) == Some(14)),
            "Match net-object → PostRound(14)"
        );
        // The match is now walking the terminal states, NOT immediately Finished.
        assert_eq!(m.phase(), FlowState::RoundEnd, "post-match walk in progress (not Finished yet)");
        assert_eq!(m.match_state_for_test(), MatchState::PostRound);

        // Further combat input is ignored during the post-match walk (RoundEnd is not
        // a combat phase; resolve only acts in StateTimeout).
        assert!(m.on_c2s(0, &[0x84, 0x36], t + Duration::from_millis(1)).is_empty());
    }

    /// The post-InRound terminal walk (the "error 3" fix): after a round-ending death
    /// the FSM advances the Match net-object's `MatchState` PostRound(14) →
    /// BackendMatchEnd(17) → PostMatch(16) → DisconnectingPlayers(19) on the s506
    /// final-round timers, then finishes — so the client shows a clean result and
    /// returns to the lobby instead of timing out at InRound. [MATCH_STATE_MATCHEND_PROGRESSION]
    #[test]
    fn post_match_state_walk_reaches_terminal_then_finishes() {
        use crate::arena::combat::state::MatchState;
        let (mut m, t0) = live_inst(2);

        // Kill B → PostRound(14), phase RoundEnd.
        let mut t = t0;
        for _ in 0..20 {
            t += Duration::from_millis(500);
            let out = m.on_c2s(0, &[0x84, 0x36], t);
            if out.iter().any(|(_, b)| b.len() > 3 && b[1] == 0x36
                && arena_proto::parse_netdata(&b[2..]).int(3) == Some(29))
            {
                break;
            }
        }
        assert_eq!(m.phase(), FlowState::RoundEnd);
        assert_eq!(m.match_state_for_test(), MatchState::PostRound);

        // Tick at 250 ms through the whole terminal walk (4 + 6 + 5 s of holds ≈ 15 s).
        // Record the MatchState updates the FSM emits, in order.
        let matchend_to_finish = Duration::from_secs(4 + 6 + 5 + 2);
        let step = Duration::from_millis(250);
        let n = (matchend_to_finish.as_millis() / step.as_millis()) as u32;
        let mut states_seen: Vec<u8> = Vec::new();
        let mut saw_terminal_heartbeat = false;
        for i in 1..=n {
            let now = t + step * i;
            let out = m.on_tick(2, now);
            for (_, b) in &out {
                // op55 Match-object update (carrier 0x35), propId5 = the new MatchState.
                if b.len() > 2 && b[1] == 0x35 {
                    if let Some(s) = arena_proto::parse_netdata(&b[2..]).int(5) {
                        if states_seen.last() != Some(&(s as u8)) {
                            states_seen.push(s as u8);
                        }
                    }
                }
                if b.ends_with(b"StateTimeout") {
                    saw_terminal_heartbeat = true;
                }
            }
            if m.phase() == FlowState::Finished {
                break;
            }
        }
        assert!(saw_terminal_heartbeat, "an op79 StateTimeout heartbeat rides the post-match walk (s506)");
        // The exact retail terminal sequence (s506 obj 123 final round): 17 → 16 → 19.
        assert_eq!(
            states_seen,
            vec![
                MatchState::BackendMatchEnd as u8,            // 17
                MatchState::PostMatch as u8,                  // 16
                MatchState::DisconnectingPlayersAfterMatch as u8, // 19
            ],
            "post-match MatchState walk must be BackendMatchEnd(17)→PostMatch(16)→Disconnecting(19), s506-exact"
        );
        assert_eq!(m.phase(), FlowState::Finished, "match Finished after the terminal state");
        assert_eq!(m.match_state_for_test(), MatchState::DisconnectingPlayersAfterMatch);
    }

    #[test]
    fn ability_cast_deals_spell_damage() {
        let (mut m, t0) = live_inst(2); // → live round

        // A casts an ability (op37 RequestExecuteAbility).
        let mut req = vec![
            0xBE, 0x36, 0x04, 0x1F, 0x70, 0x77, 0x0A, 0x35, 0x02, 0x00, 0x00, 0x38, 0x03, 0x25, 0x24, 0x00,
        ];
        req.extend_from_slice(b"7fc15804-1637-40a9-8dcc-3ea1eb0f778d");
        let out = m.on_c2s(0, &req, t0);

        // PerformExecuteAbility (38) echoed (gmid byte at sep+5 = index 13).
        assert!(out.iter().any(|(_, b)| b.get(13) == Some(&38)), "PerformExecuteAbility echoed");
        // A ReceiveDamage with Spell source (propId 6 = 2).
        let rd = out
            .iter()
            .find(|(_, b)| b[1] == 0x36 && arena_proto::parse_netdata(&b[2..]).int(3) == Some(50))
            .expect("ReceiveDamage present");
        assert_eq!(arena_proto::parse_netdata(&rd.1[2..]).int(6), Some(2), "Spell damage source");

        // The same ability is on cooldown immediately after.
        assert!(m.on_c2s(0, &req, t0).is_empty(), "ability on cooldown");
    }

    /// DEBUG-HOLD (`ARENA_DEBUG_HOLD`): the FULL round-start burst still goes out
    /// (Connecting→Spawning→BackendMatchCreated) but the match then FREEZES at
    /// BackendMatchCreated forever — it never advances to the live round
    /// (StateTimeout), no matter how much time elapses. (No bot can swing because
    /// StateTimeout is never entered; the resolve guard is the belt-and-suspenders.)
    #[test]
    fn debug_hold_freezes_at_backend_match_created() {
        let now = Instant::now();
        // Solo-vs-bot shape (the prod repro: capacity 2, one real peer expected).
        let mut m = MatchInstance::new(2, 1, vec![], now);
        m.set_debug_hold(true);

        m.on_tick(1, now); // Connecting → Spawning (full spawn/profile burst)
        assert_eq!(m.phase(), FlowState::Spawning);
        let created = m.on_tick(1, now + SPAWN_HANDSHAKE_HOLD); // Spawning → BackendMatchCreated
        assert_eq!(m.phase(), FlowState::BackendMatchCreated);
        assert_eq!(
            created.iter().filter(|(_, b)| b.ends_with(b"BackendMatchCreated")).count(),
            2,
            "the BackendMatchCreated round-start frame is still emitted under HOLD"
        );

        // Way past every normal hold/heartbeat (the full setup walk + a minute): still
        // BackendMatchCreation(5), never advances the MatchState past 5, never enters
        // StateTimeout, and the tick produces NOTHING (no MatchState update, no flow
        // heartbeat, no bot) — the freeze window for hand-injecting s2c frames.
        let later = now + setup_to_live() + Duration::from_secs(60);
        let out = m.on_tick(1, later);
        assert_eq!(m.phase(), FlowState::BackendMatchCreated, "HOLD never advances to the live round");
        assert!(out.is_empty(), "no s2c is generated while held at BackendMatchCreated");
        assert_eq!(m.combat.round, 0, "round never goes live under HOLD");
    }

    /// With HOLD on, the bot does NOT damage the player even if the match is somehow
    /// in the live round — `resolve::on_tick` short-circuits on the flag (the
    /// belt-and-suspenders guard from point 2 of the DEBUG-HOLD change).
    #[test]
    fn debug_hold_suppresses_bot_swings() {
        let now = Instant::now();
        let mut m = MatchInstance::new(2, 1, vec![], now);
        m.set_debug_hold(true);
        // Force the live round directly (bypassing the FSM hold) to prove the
        // resolve-side guard independently: drive the phase, then tick well past the
        // bot cadence. The player must take ZERO damage.
        m.combat.phase = FlowState::StateTimeout;
        m.combat.phase_entered = now;
        let full = m.fighter_health(0);
        let out = m.on_tick(1, now + Duration::from_secs(10));
        assert_eq!(m.fighter_health(0), full, "no bot swing damages the player under HOLD");
        // The only s2c on a held StateTimeout tick is the flow heartbeat (carrier
        // 0x36, propId3 == 0x4F); there must be NO ReceiveDamage (carrier 0x36,
        // propId3 == 50) from a bot swing.
        assert!(
            !out.iter().any(|(_, b)| b.len() > 2
                && b[1] == 0x36
                && arena_proto::parse_netdata(&b[2..]).int(3) == Some(50)),
            "no ReceiveDamage (bot swing) emitted under HOLD"
        );
    }

    /// Round-start op50 spawn OBJECT SET is retail-faithful to s506: the local viewer
    /// receives a **Player** op50 for BOTH fighters (WaitingForPlayers burst) and, after
    /// the Match reaches InitialPlayerSetup(4), an **Avatar** op50 for BOTH fighters —
    /// its OWN (Autonomous) AND the OPPONENT's (Simulated). The Simulated opponent Avatar
    /// (s506 obj 125, role 2) is what flips `HasOpponentPlayer`: the client's
    /// `PvpEncounter.SpawnOpponent` looks the avatar's char-UUID up in `_pvpPlayers`
    /// (`GetPvpPlayer`) and sets `_opponentInfo.Player`. Proven on-device 2026-06-19:
    /// injecting the missing Simulated avatar flipped `HasOpponentPlayer` 0→1. Guards the
    /// fix that REVERSED the earlier (wrong) "own-Avatar-only" round-start.
    #[test]
    fn round_start_spawns_both_avatars() {
        let now = Instant::now();
        let mk = |name: &str, uuid: &str| {
            let mut l = crate::arena::combat::loadout::starter();
            l.display_name = name.to_string();
            l.character_uuid = uuid.to_string();
            l.profile_equipped_json = r#"{"equippedItems":{}}"#.to_string();
            l.profile_character_json = format!(r#"{{"name":"{name}"}}"#);
            l
        };
        // capacity 2, expected_peers 1 — the matchmaker's solo-vs-ghost shape.
        let mut m = MatchInstance::new(
            2,
            1,
            vec![
                mk("WolfWalker", "38c987fd-c42b-4ea6-b869-c8d4c03055f9"),
                mk("Blank", "1131a037-716c-49cc-b165-32d8ddc14f49"),
            ],
            now,
        );
        // First burst: Players + Match (no avatars yet).
        let burst = m.on_tick(1, now);
        let burst_player_roles: Vec<_> = burst
            .iter()
            .filter(|(v, b)| *v == 0 && b.len() >= 3 && b[1] == 0x32)
            .filter_map(|(_, b)| {
                let nd = arena_proto::parse_netdata(&b[2..]);
                (nd.int(1) == Some(55)).then(|| nd.int(2))
            })
            .collect();
        assert!(
            burst_player_roles.contains(&Some(3)) && burst_player_roles.contains(&Some(2)),
            "viewer 0 gets a Player op50 for both fighters (roles seen: {burst_player_roles:?})"
        );
        // The 3→4 tick: BOTH avatars (own Autonomous=3 AND opponent Simulated=2).
        let setup = m.on_tick(1, now + MATCH_SETUP_STAGGER);
        let mut avatar_roles: Vec<_> = setup
            .iter()
            .filter(|(v, b)| *v == 0 && b.len() >= 3 && b[1] == 0x32)
            .filter_map(|(_, b)| {
                let nd = arena_proto::parse_netdata(&b[2..]);
                (nd.int(1) == Some(56)).then(|| nd.int(2))
            })
            .collect();
        avatar_roles.sort();
        assert_eq!(
            avatar_roles,
            vec![Some(2), Some(3)],
            "viewer 0 gets BOTH Avatar op50: own (Autonomous=3) AND opponent (Simulated=2)"
        );
    }
}
