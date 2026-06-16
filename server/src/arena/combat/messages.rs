//! s2c message builders — produce the exact `user_data` bytes the retail client
//! expects, using `arena_proto::netdata`.
//!
//! A built message is the decrypted SEND payload: `marker(0xBE) ‖ MessageType ‖
//! body`. The match layer encrypts it under the target peer's key and hands it to
//! ENet (`match_registry::handle_live_user_data` / the tick path).
//!
//! **MessageType (`user_data[1]`) is a carrier, not the GameMessageId** (see the
//! module docs): the flow-control stateName messages and the swipe/ability/damage
//! family all ride MessageType `0x36` (the "UserMessage" carrier); the real
//! GameMessage is disambiguated structurally by the body. `CombatScreenInfo` uses
//! its own carrier `0x37`.
//!
//! Every builder here has a byte-for-byte test against a real session-293 frame.

use arena_proto::{GameMessageId, NetDataWriter};

use super::state::{ActiveSide, DamageSource, DamageType, FlowState, NetObjectType, NetRole};

/// `NetTransportMessage.MAGIC_HEADER` — present on every message, both directions.
pub const MARKER_S2C: u8 = 0xBE;

/// Carrier MessageType for the "UserMessage" family (flow stateName, swipe,
/// ability, damage — disambiguated by body structure).
pub const MSGTYPE_USERMESSAGE: u8 = 0x36; // 54
/// Carrier MessageType for `CombatScreenInfo`.
pub const MSGTYPE_COMBAT_SCREEN: u8 = 0x37; // 55

/// firstPropId selector for a stateName payload: `0x4F` server→client,
/// `0x50` client→server (the echo). The server only emits the s2c form.
const STATE_SELECTOR_S2C: u8 = 0x4F;

/// Wrap a NetData `body` as a complete s2c `user_data`: `0xBE ‖ msg_type ‖ body`.
fn frame(msg_type: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + body.len());
    out.push(MARKER_S2C);
    out.push(msg_type);
    out.extend_from_slice(&body);
    out
}

/// A flow-control stateName message on the match flow-controller net object —
/// e.g. `BackendMatchCreated`, `StateTimeout`, `NextState`, `RoundEnd`. This is
/// how the server drives the match/round state machine (server-authoritative,
/// `NetRole::None` on the controller, selector `0x4F`).
///
/// `flow_controller_id` is the Control net object the server assigns for the
/// match (s293 used 436). Returns `None` for the synthetic [`FlowState`]s that
/// have no wire string (`Connecting`/`Finished`).
pub fn flow_state(flow_controller_id: i32, state: FlowState) -> Option<Vec<u8>> {
    let name = state.wire_name()?;
    let mut w = NetDataWriter::new();
    w.int(0, flow_controller_id)
        .byte(1, NetObjectType::Control as u8)
        .byte(2, NetRole::None as u8)
        .byte(3, STATE_SELECTOR_S2C)
        .string(4, name);
    Some(frame(MSGTYPE_USERMESSAGE, w.finish()))
}

/// A `CombatScreenInfo` (op55) — a lightweight per-net-object signal carrying
/// only NetObjectInfo (no payload). Emitted for the relevant player/avatar
/// objects as the combat screen comes up.
pub fn combat_screen_info(net_object_id: i32, net_object_type: NetObjectType, role: NetRole) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.net_object_info(net_object_id, net_object_type as u8, role as u8);
    frame(MSGTYPE_COMBAT_SCREEN, w.finish())
}

/// Carrier MessageType for a net-object **SPAWN** (op `0x32` = 50) — the generic
/// object-registration message the server sends at round start so the client can
/// construct each Player/Avatar/Match object. Decoded from two-sided + Taheen
/// captures; see `docs/arena-protocol-spec.md` §6.2 and `docs/arena-journey-log.md` §6.
pub const MSGTYPE_SPAWN: u8 = 0x32; // 50

/// Spawn a **Player** net-object (the per-player object the client renders + names).
/// `role`: [`NetRole::Autonomous`] (3) for the viewer's OWN player, [`NetRole::Simulated`]
/// (2) for the opponent. `rank_a`/`rank_b` are the two trailing ints (arena rank/index —
/// captured 72/72 for Taheen, 6/7 for flapdroid; exact meaning TBD, non-fatal to render).
/// Byte-verified against session-486 (Taheen) frame.
pub fn spawn_player(
    net_object_id: i32,
    role: NetRole,
    name: &str,
    character_uuid: &str,
    rank_a: i32,
    rank_b: i32,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, net_object_id)
        .byte(1, NetObjectType::Player as u8)
        .byte(2, role as u8)
        .string(3, name)
        .string(4, character_uuid)
        .int(5, rank_a)
        .int(6, rank_b);
    frame(MSGTYPE_SPAWN, w.finish())
}

/// Spawn an **Avatar** net-object (the in-arena fighter body). Sparse NetData
/// (props 0,1,2,4 — no display name); links to the character UUID for appearance/gear.
pub fn spawn_avatar(net_object_id: i32, role: NetRole, character_uuid: &str) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, net_object_id)
        .byte(1, NetObjectType::Avatar as u8)
        .byte(2, role as u8)
        .string(4, character_uuid);
    frame(MSGTYPE_SPAWN, w.finish())
}

/// Build a `ReceiveDamage` — the s2c authoritative damage event. Carrier
/// MessageType `0x36` (54); the real GameMessageId (50) lives at NetData propId 3
/// (carrier 54 is shared with swipe/ability/etc.). The message describes the
/// `damaged` actor: propId 4 = its packed pools post-hit, propId 5 = the
/// opponent's. `total_damage` is the sum of the health-affecting component values
/// (stat-drain types — Stamina/Magicka — are listed as components but excluded
/// from the total); the caller's `DamageModel` computes both. Byte-verified
/// against session-293 frame 1956589.
#[allow(clippy::too_many_arguments)]
pub fn receive_damage(
    damaged_net_object_id: i32,
    damaged_net_object_type: u8,
    damaged_packed_stats: u64,
    other_packed_stats: u64,
    source: DamageSource,
    flags: u8,
    total_damage: f32,
    combo: i16,
    active_side: ActiveSide,
    most_resisted: DamageType,
    components: &[(DamageType, f32)],
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.net_object_info(damaged_net_object_id, damaged_net_object_type, NetRole::Authority as u8)
        .byte(3, 50) // gameMessageId = ReceiveDamage (the real discriminator)
        .ulong(4, damaged_packed_stats)
        .ulong(5, other_packed_stats)
        .byte(6, source as u8)
        .byte(7, flags)
        .float(8, total_damage)
        .int16(9, combo)
        .byte(10, active_side as u8)
        .byte(11, most_resisted as u8)
        .byte(12, components.len() as u8);
    for (k, (ty, val)) in components.iter().enumerate() {
        let base = 13 + 2 * k as u8;
        w.byte(base, *ty as u8).float(base + 1, *val);
    }
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// `PlayerDeadStateChange` (29) — the addressed avatar died.
///
/// **Layout UNVERIFIED:** op29 never appears in our captures; modeled on the
/// PlayerStateChange NetObjectInfo shape, carrier = the GameMessageId itself
/// (non-overloaded messages use their id as `user_data[1]`). Refine when a
/// death/round-boundary capture lands.
pub fn player_dead(net_object_id: i32) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.net_object_info(net_object_id, NetObjectType::Avatar as u8, NetRole::Authority as u8);
    frame(GameMessageId::PlayerDeadStateChange as u8, w.finish())
}

/// `MatchEndMatchMsg` (49) — the match concluded; `winner_net_object_id` won.
///
/// **Layout UNVERIFIED:** op49 carries a large fragmented `ResultsJSON` and was
/// never captured as a walkable command; modeled minimally (match NetObjectInfo +
/// the winning avatar id at propId 3). Refine on capture; keep ResultsJSON minimal.
pub fn match_end(winner_net_object_id: i32) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.net_object_info(winner_net_object_id, NetObjectType::Match as u8, NetRole::Authority as u8)
        .int(3, winner_net_object_id);
    frame(GameMessageId::MatchEndMatchMsg as u8, w.finish())
}

/// `PerformExecuteAbility` (38) — the s2c echo of a `RequestExecuteAbility` (37).
/// Byte-identical to the request except the s2c marker (`0xBE`), NetRole=Authority,
/// and gameMessageId=38 (`arena-combat-reference.md` §op37/38). Built by patching
/// the client's OWN request bytes, so it faithfully mirrors whatever NetObjectInfo
/// framing the client sent. `sep_offset` is the `02 00 00` separator offset that
/// the decoder ([`super::input::parse_execute_ability`]) located.
pub fn perform_execute_ability(request_user_data: &[u8], sep_offset: usize) -> Vec<u8> {
    let mut echo = request_user_data.to_vec();
    if let Some(b) = echo.first_mut() {
        *b = MARKER_S2C;
    }
    if sep_offset + 5 < echo.len() {
        echo[sep_offset + 4] = NetRole::Authority as u8; // role → Authority
        echo[sep_offset + 5] = GameMessageId::PerformExecuteAbility as u8; // gmid 37 → 38
    }
    echo
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-for-byte vs session-293 frame 1955434 (s2c BackendMatchCreated):
    /// user_data = `BE 36 04 1F 7077 0A B4010000 39 00 4F 1300 "BackendMatchCreated"`.
    #[test]
    fn flow_state_matches_capture() {
        let got = flow_state(436, FlowState::BackendMatchCreated).unwrap();
        let mut want = vec![
            0xBE, 0x36, // marker + UserMessage carrier
            0x04, 0x1F, // maxPropId=4, bitmap {0,1,2,3,4}
            0x70, 0x77, 0x0A, // type nibbles [Int,Byte,Byte,Byte,String]
            0xB4, 0x01, 0x00, 0x00, // prop0 Int = 436 (flow controller)
            0x39, // prop1 Byte = 57 (Control)
            0x00, // prop2 Byte = 0 (NetRole::None)
            0x4F, // prop3 Byte = 0x4F (stateName selector, s2c)
            0x13, 0x00, // prop4 String len = 19
        ];
        want.extend_from_slice(b"BackendMatchCreated");
        assert_eq!(got, want);
    }

    #[test]
    fn flow_state_other_states_build() {
        for (st, name) in [
            (FlowState::StateTimeout, "StateTimeout"),
            (FlowState::NextState, "NextState"),
            (FlowState::RoundEnd, "RoundEnd"),
        ] {
            let m = flow_state(436, st).unwrap();
            assert_eq!(&m[0..2], &[0xBE, 0x36]);
            // the stateName string is present at the tail
            assert!(m.ends_with(name.as_bytes()));
        }
        assert!(flow_state(436, FlowState::Connecting).is_none());
    }

    /// Byte-for-byte vs session-293 frame 1955386's first command (c2s
    /// CombatScreenInfo): user_data = `BE 37 02 07 7007 B5010000 37 02`
    /// (NetObjectInfo id=437, type=55 Player, role=2 Simulated).
    #[test]
    fn combat_screen_info_matches_capture() {
        let got = combat_screen_info(437, NetObjectType::Player, NetRole::Simulated);
        assert_eq!(
            got,
            &[0xBE, 0x37, 0x02, 0x07, 0x70, 0x07, 0xB5, 0x01, 0x00, 0x00, 0x37, 0x02]
        );
    }

    /// Byte-for-byte vs session-486 (Taheen) op50 Player spawn: net_obj 197,
    /// role Autonomous(3), name "Taheen", char bee74bea-…, ranks 72/72.
    #[test]
    fn spawn_player_matches_capture() {
        let got = spawn_player(
            197,
            NetRole::Autonomous,
            "Taheen",
            "bee74bea-1ab5-46c0-9eb5-f81e6e25ac05",
            72,
            72,
        );
        let mut want = vec![
            0xBE, 0x32, // marker + SPAWN carrier (50)
            0x06, // maxPropId = 6
            0x7F, // bitmap: props 0..6 present
            0x70, 0xA7, 0x0A, 0x00, // type nibbles [Int,Byte,Byte,String,String,Int,Int]
            0xC5, 0x00, 0x00, 0x00, // p0 netObjectId = 197
            0x37, // p1 = 55 (Player)
            0x03, // p2 = 3 (Autonomous = self)
            0x06, 0x00, // p3 String len = 6
        ];
        want.extend_from_slice(b"Taheen");
        want.extend_from_slice(&[0x24, 0x00]); // p4 String len = 36
        want.extend_from_slice(b"bee74bea-1ab5-46c0-9eb5-f81e6e25ac05");
        want.extend_from_slice(&[0x48, 0, 0, 0, 0x48, 0, 0, 0]); // p5,p6 = 72,72
        assert_eq!(got, want);
    }

    /// op50 Avatar spawn is sparse (props 0,1,2,4 — no name). s486 net_obj 200.
    #[test]
    fn spawn_avatar_is_sparse() {
        let got = spawn_avatar(200, NetRole::Autonomous, "bee74bea-1ab5-46c0-9eb5-f81e6e25ac05");
        assert_eq!(&got[0..2], &[0xBE, 0x32], "marker + spawn carrier");
        assert_eq!(got[2], 0x04, "maxPropId = 4");
        assert_eq!(got[3], 0x17, "bitmap = props {{0,1,2,4}}");
        assert_eq!(&got[4..6], &[0x70, 0xA7], "type nibbles [Int,Byte,Byte,String]");
        assert!(got.ends_with(b"bee74bea-1ab5-46c0-9eb5-f81e6e25ac05"));
    }

    /// Byte-for-byte vs session-293 frame 1956589 (s2c ReceiveDamage): an Attack
    /// on the Left side, total 85.172 = Slashing 60.731 + Shock 24.441, with an
    /// equal Magicka drain (excluded from the total). Uses the exact captured f32
    /// bit patterns so the encode is provably identical to the retail client's.
    #[test]
    fn receive_damage_matches_capture() {
        let total = f32::from_le_bytes([0x12, 0x58, 0xAA, 0x42]); // 85.172
        let slashing = f32::from_le_bytes([0x8A, 0xEC, 0x72, 0x42]); // 60.731
        let shock = f32::from_le_bytes([0x36, 0x87, 0xC3, 0x41]); // 24.441
        let magicka = shock; // mirrored drain
        let got = receive_damage(
            65,
            NetObjectType::Avatar as u8,
            0x39df_ff92_0000_0024, // this(damaged): stat word in hi32 (→ Health 914), seq 36 in lo32
            0x3fff_ffff_0000_0024, // other(attacker): stat word 0x3fffffff (Health 1023, full)
            DamageSource::Attack,
            0x03, // ShowDamage | HasAttacker
            total,
            0,
            ActiveSide::Left,
            DamageType::None,
            &[
                (DamageType::Slashing, slashing),
                (DamageType::Shock, shock),
                (DamageType::Magicka, magicka),
            ],
        );
        let want: &[u8] = &[
            0xBE, 0x36, // marker + UserMessage carrier (54)
            0x12, // maxPropId = 18
            0xFF, 0xFF, 0x07, // bitmap: props 0..18 present
            0x70, 0x77, 0x22, 0x77, 0x85, 0x77, 0x77, 0x75, 0x75, 0x05, // type nibbles
            0x41, 0x00, 0x00, 0x00, // p0 netObjectId = 65
            0x38, // p1 type = 56 (Avatar)
            0x01, // p2 role = 1 (Authority)
            0x32, // p3 gameMessageId = 50
            0x24, 0x00, 0x00, 0x00, 0x92, 0xFF, 0xDF, 0x39, // p4 thisStats
            0x24, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x3F, // p5 otherStats
            0x01, // p6 damageSource = Attack
            0x03, // p7 flags
            0x12, 0x58, 0xAA, 0x42, // p8 totalDamage = 85.172
            0x00, 0x00, // p9 comboCount = 0
            0x02, // p10 activeSide = Left
            0x00, // p11 mostResisted = None
            0x03, // p12 numDamageTypes = 3
            0x01, 0x8A, 0xEC, 0x72, 0x42, // p13/14 Slashing 60.731
            0x06, 0x36, 0x87, 0xC3, 0x41, // p15/16 Shock 24.441
            0x09, 0x36, 0x87, 0xC3, 0x41, // p17/18 Magicka 24.441
        ];
        assert_eq!(got, want);
    }

    /// PerformExecuteAbility (38) is the request (37) with the marker, role, and
    /// gameMessageId patched — everything else (incl. the ability UUID) preserved.
    #[test]
    fn perform_execute_ability_echoes_request() {
        let mut req = vec![
            0xBE, 0x36, 0x04, 0x1F, 0x70, 0x77, 0x0A, 0x35, // marker+carrier + NetObjectInfo
            0x02, 0x00, 0x00, // separator @ offset 8
            0x38, 0x03, 0x25, 0x24, 0x00, // type, role=3, gmid=37, len=36
        ];
        req.extend_from_slice(b"7fc15804-1637-40a9-8dcc-3ea1eb0f778d");
        let echo = perform_execute_ability(&req, 8);
        assert_eq!(echo[0], 0xBE, "s2c marker");
        assert_eq!(echo[12], NetRole::Authority as u8, "role → Authority (sep+4)");
        assert_eq!(echo[13], 38, "gameMessageId → PerformExecuteAbility (sep+5)");
        assert_eq!(&echo[16..], b"7fc15804-1637-40a9-8dcc-3ea1eb0f778d", "UUID preserved");
        assert_eq!(echo.len(), req.len());
    }
}
