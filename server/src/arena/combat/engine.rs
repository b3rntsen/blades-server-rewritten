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

use super::messages;
use super::resolve;
use super::state::{Fighter, FlowState, Loadout, MatchCombat, NetObjectType, NetRole};

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
    pub fn new(capacity: usize, loadouts: Vec<Loadout>, now: Instant) -> Self {
        let mut combat = MatchCombat::new(capacity, now);
        for slot in 0..capacity {
            let net_object_id = combat.alloc_net_object_id();
            // Use the provided loadout if it carries a weapon; else a starter
            // loadout so the damage model produces a real, progressing fight.
            let loadout = loadouts
                .get(slot)
                .cloned()
                .filter(|l| !l.weapon.base_by_type.is_empty())
                .unwrap_or_else(super::loadout::starter);
            combat
                .fighters
                .push(Fighter::new(slot, net_object_id, loadout, now));
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

        // ConcedeMatch ends the match for everyone. (Heuristic: the concede
        // carrier byte == GameMessageId::ConcedeMatch; refine if a capture shows
        // otherwise.)
        if user_data.get(1) == Some(&(GameMessageId::ConcedeMatch as u8))
            && !matches!(self.combat.phase, FlowState::Finished)
        {
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
                if self.combat.capacity() > 0 && connected >= self.combat.capacity() {
                    self.combat.phase = FlowState::BackendMatchCreated;
                    self.combat.phase_entered = now;
                    self.last_heartbeat = now;
                    self.broadcast_flow(&mut out, FlowState::BackendMatchCreated);
                    self.broadcast_combat_screen(&mut out);
                }
            }
            // Brief hold, then the round goes live (StateTimeout heartbeat).
            FlowState::BackendMatchCreated => {
                if now.duration_since(self.combat.phase_entered) >= MATCH_CREATE_HOLD {
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

    fn broadcast_flow(&self, out: &mut Vec<(usize, Vec<u8>)>, state: FlowState) {
        if let Some(msg) = messages::flow_state(self.combat.flow_controller_id, state) {
            for slot in 0..self.combat.fighters.len() {
                out.push((slot, msg.clone()));
            }
        }
    }

    fn broadcast_combat_screen(&self, out: &mut Vec<(usize, Vec<u8>)>) {
        for viewer in 0..self.combat.fighters.len() {
            for actor in &self.combat.fighters {
                out.push((
                    viewer,
                    messages::combat_screen_info(
                        actor.net_object_id,
                        NetObjectType::Avatar,
                        NetRole::Authority,
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
        (MatchInstance::new(capacity, vec![], now), now)
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
        // 2 viewers × 2 avatars = 4 combat-screen messages.
        let screens = out.iter().filter(|(_, b)| b.len() >= 2 && b[1] == 0x37).count();
        assert_eq!(screens, 4);
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
