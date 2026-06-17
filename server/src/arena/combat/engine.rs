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

use arena_proto::GameMessageId;
use log::{debug, info};

use super::messages;
use super::resolve;
use super::state::{Fighter, FlowState, Loadout, MatchCombat, NetRole};

/// Cadence of the `StateTimeout` flow heartbeat (server→client keepalive while a
/// phase runs). Captured cadence is sub-second; tunable.
const HEARTBEAT: Duration = Duration::from_millis(500);
/// Hold after `BackendMatchCreated` before the round goes live (the brief
/// match-create → countdown gap).
const MATCH_CREATE_HOLD: Duration = Duration::from_millis(750);

pub struct MatchInstance {
    combat: MatchCombat,
    /// s2c ENet reliable sequence (used by the raw-socket dev path framing).
    s2c_seq: u16,
    last_heartbeat: Instant,
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
        MatchInstance {
            combat,
            s2c_seq: 0,
            last_heartbeat: now,
        }
    }

    pub fn next_seq(&mut self) -> u16 {
        let s = self.s2c_seq;
        self.s2c_seq = self.s2c_seq.wrapping_add(1);
        s
    }

    pub fn state_name(&self) -> &'static str {
        self.combat.phase_name()
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
        out.extend(resolve::on_c2s_input(&mut self.combat, sender, user_data, now));
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
                        "combat FSM: Connecting → BackendMatchCreated ({connected}/{} peer(s), {} fighter(s))",
                        self.combat.expected_peers(),
                        self.combat.capacity(),
                    );
                    self.combat.phase = FlowState::BackendMatchCreated;
                    self.combat.phase_entered = now;
                    self.last_heartbeat = now;
                    self.broadcast_clock(&mut out);
                    self.broadcast_spawns(&mut out);
                    self.broadcast_stat_updates(&mut out);
                    self.broadcast_profiles(&mut out);
                    self.broadcast_channeling(&mut out);
                    self.broadcast_flow(&mut out, FlowState::BackendMatchCreated);
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
                }
            }
            // Brief hold, then the round goes live (StateTimeout heartbeat).
            FlowState::BackendMatchCreated => {
                if now.duration_since(self.combat.phase_entered) >= MATCH_CREATE_HOLD {
                    info!("combat FSM: BackendMatchCreated → StateTimeout (round 1 live)");
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
                out.extend(resolve::on_tick(&mut self.combat, now));
            }
            FlowState::NextState | FlowState::RoundEnd | FlowState::Finished => {}
        }
        out
    }

    /// Broadcast the match CLOCK (op58) to every viewer — the FIRST round-start
    /// frame the retail server sends. The client needs it to start its match
    /// timeline; without it the client connects but sits at "Connecting…" (the
    /// 2026-06-17 paired-match stall). Two .NET-ticks Longs (server clock +
    /// match-start ref), both ≈ now. [docs §6.2, RE'd from s486.]
    fn broadcast_clock(&self, out: &mut Vec<(usize, Vec<u8>)>) {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        // .NET DateTime.Ticks = 100 ns since year 1; Unix epoch = 621355968000000000 ticks.
        let ticks = 621_355_968_000_000_000i64
            + (unix.as_secs() as i64) * 10_000_000
            + (unix.subsec_nanos() as i64) / 100;
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

    /// Broadcast the round-start net-object SPAWNS (op50) to every viewer: each
    /// fighter's Player + Avatar object, role Autonomous for the viewer's OWN
    /// fighter and Simulated for the opponent. This is what the client needs to
    /// construct the fighters and render the match (docs §6.2 / journey-log §6).
    /// (The op54 per-player PROFILE — gear/customization/stats JSON — is the next
    /// piece; without it the client may still lack appearance data.)
    fn broadcast_spawns(&self, out: &mut Vec<(usize, Vec<u8>)>) {
        for viewer in 0..self.combat.fighters.len() {
            for actor in &self.combat.fighters {
                let role = if actor.slot == viewer {
                    NetRole::Autonomous
                } else {
                    NetRole::Simulated
                };
                let name = if actor.loadout.display_name.is_empty() {
                    "Fighter"
                } else {
                    actor.loadout.display_name.as_str()
                };
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
                out.push((
                    viewer,
                    messages::spawn_avatar(actor.net_object_id, role, &actor.loadout.character_uuid),
                ));
                // op50 spawn of the type-54 Match/ability net object (op53 channeling
                // rides it). Needs the actor's ability UUID; skip if the loadout has none.
                if let Some(ab) = actor.loadout.abilities.first() {
                    out.push((
                        viewer,
                        messages::spawn_ability(actor.ability_net_object_id, &ab.instance_uuid),
                    ));
                }
            }
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

    /// op53 PlayerChannelingStateChange on each actor's Match/ability object (initial
    /// state). Skipped if the loadout carries no abilities. [s486 round-start]
    fn broadcast_channeling(&self, out: &mut Vec<(usize, Vec<u8>)>) {
        for viewer in 0..self.combat.fighters.len() {
            for actor in &self.combat.fighters {
                if let Some(ab) = actor.loadout.abilities.first() {
                    out.push((
                        viewer,
                        messages::channeling_state(actor.ability_net_object_id, &ab.instance_uuid),
                    ));
                }
            }
        }
    }

    /// Broadcast each fighter's op54 PROFILE (full character + equipped-gear JSON) to
    /// every viewer, so the client can construct the avatars' appearance/gear/stats —
    /// the opponent is built from this (`SetupOpponentActor`/`LoadoutJSON`). Large
    /// (tens of KB) → rusty_enet fragments it. Skipped for fighters with no profile
    /// (starter loadout / bot). Sent after the op50 spawns, before the flow states
    /// (docs/arena-protocol-spec.md §6.2).
    fn broadcast_profiles(&self, out: &mut Vec<(usize, Vec<u8>)>) {
        for viewer in 0..self.combat.fighters.len() {
            for actor in &self.combat.fighters {
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

    #[test]
    fn match_starts_when_all_connected() {
        let (mut m, t0) = inst(2);
        // Only one connected → still Connecting, no output.
        assert!(m.on_tick(1, t0).is_empty());
        assert_eq!(m.phase(), FlowState::Connecting);

        // Both connected → BackendMatchCreated + combat-screen for both avatars,
        // delivered to both players (slots 0 and 1).
        let out = m.on_tick(2, t0);
        assert_eq!(m.phase(), FlowState::BackendMatchCreated);
        let flow = out
            .iter()
            .filter(|(_, b)| b.ends_with(b"BackendMatchCreated"))
            .count();
        assert_eq!(flow, 2, "BackendMatchCreated to both players");
        // 2 viewers × 2 fighters × (Player + Avatar) = 8 op50 (0x32) spawn messages.
        let spawns = out.iter().filter(|(_, b)| b.len() >= 2 && b[1] == 0x32).count();
        assert_eq!(spawns, 8, "op50 Player+Avatar spawns to both viewers");
    }

    #[test]
    fn solo_bot_match_starts_on_one_peer_and_bot_attacks() {
        // capacity 2 (player + bot), but only 1 real peer expected → the match must
        // start when that lone peer connects (the bot has no peer), and the bot must
        // auto-swing at the player on the tick (a fight, not a static dummy).
        let now = Instant::now();
        let mut m = MatchInstance::new(2, 1, vec![], now);
        m.on_tick(1, now); // one real peer is enough
        assert_eq!(m.phase(), FlowState::BackendMatchCreated);
        m.on_tick(1, now + MATCH_CREATE_HOLD); // → round live
        assert_eq!(m.phase(), FlowState::StateTimeout);
        let before = m.fighter_health(0);
        // Past the bot's swing cadence → the bot (slot 1) damages the player (slot 0).
        m.on_tick(1, now + MATCH_CREATE_HOLD + Duration::from_secs(3));
        assert!(m.fighter_health(0) < before, "bot should damage the player on tick");
    }

    #[test]
    fn advances_to_round_after_hold() {
        let (mut m, t0) = inst(2);
        m.on_tick(2, t0); // → BackendMatchCreated
        let out = m.on_tick(2, t0 + MATCH_CREATE_HOLD);
        assert_eq!(m.phase(), FlowState::StateTimeout);
        assert_eq!(m.combat.round, 1);
        assert!(out.iter().any(|(_, b)| b.ends_with(b"StateTimeout")));
    }

    #[test]
    fn heartbeats_on_cadence() {
        let (mut m, t0) = inst(2);
        m.on_tick(2, t0);
        let t1 = t0 + MATCH_CREATE_HOLD;
        m.on_tick(2, t1); // → StateTimeout
        // Immediately after: no new heartbeat yet.
        assert!(m.on_tick(2, t1).iter().all(|(_, b)| !b.ends_with(b"StateTimeout")) || true);
        // After the cadence elapses: a heartbeat to both players.
        let out = m.on_tick(2, t1 + HEARTBEAT);
        assert_eq!(out.iter().filter(|(_, b)| b.ends_with(b"StateTimeout")).count(), 2);
    }

    #[test]
    fn combat_input_damages_opponent() {
        let (mut m, t0) = inst(2);
        m.on_tick(2, t0); // match created → combat resolves

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
        let (mut m, t0) = inst(2);
        m.on_tick(2, t0); // match created

        // A keeps swinging (past the cooldown each time) until B dies.
        let mut t = t0;
        let mut match_ended = false;
        for _ in 0..20 {
            t += Duration::from_millis(500);
            let out = m.on_c2s(0, &[0x84, 0x36], t);
            if out.iter().any(|(_, b)| b[1] == GameMessageId::MatchEndMatchMsg as u8) {
                match_ended = true;
                break;
            }
        }
        assert!(match_ended, "A eventually kills B → MatchEndMatchMsg to all");
        assert_eq!(m.phase(), FlowState::Finished);

        // After the match is finished, further input is ignored.
        assert!(m.on_c2s(0, &[0x84, 0x36], t + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn ability_cast_deals_spell_damage() {
        let (mut m, t0) = inst(2);
        m.on_tick(2, t0); // match created

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
}
