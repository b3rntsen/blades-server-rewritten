//! Decoders for inbound c2s combat inputs (carrier MessageType `0x36` = 54).
//!
//! Carrier 54 is shared by the swipe-input, position, and ability-request messages
//! (and more); they're disambiguated structurally. This module decodes the ones
//! the engine acts on. Layouts: `docs/archive/arena-combat-reference.md`.

/// A decoded `RequestExecuteAbility` (37): the ability *instance* UUID being cast
/// and the byte offset of its role (so the echo `PerformExecuteAbility` can patch
/// role+gmid in place without depending on the numeric Avatar id).
#[derive(Debug, Clone, PartialEq)]
pub struct ExecuteAbility {
    pub role_offset: usize,
    pub ability_uuid: String,
}

/// Detect + decode a `RequestExecuteAbility` (37) in a carrier-54 c2s `user_data`.
/// The body is NetData (`arena-combat-reference.md` §op37):
/// `{0:Int AvatarId, 1:Byte Avatar, 2:Byte Autonomous, 3:Byte 37,
/// 4:String abilityUuid}`. Captures used ids whose upper little-endian bytes happened
/// to be `02 00 00`; those bytes are part of propId 0, not a structural separator.
/// Returns `None` if the frame isn't an ability request (e.g. a swipe input).
pub fn parse_execute_ability(user_data: &[u8]) -> Option<ExecuteAbility> {
    if user_data.get(1) != Some(&0x36) {
        return None;
    }
    let nd = arena_proto::parse_netdata(user_data.get(2..)?);
    if !nd.ok || nd.int(1) != Some(56) || nd.int(2) != Some(3) || nd.int(3) != Some(37) {
        return None;
    }
    let ability_uuid = nd.string(4)?.to_string();
    if ability_uuid.len() != 36 {
        return None;
    }

    // Locate the canonical p1..p4 value tail only to recover the two mutable byte
    // offsets. The semantic validation above comes from the decoder, so this scan is
    // independent of propId 0's integer bytes and cannot mistake an Avatar id for a
    // separator.
    let role_offset = user_data
        .windows(5 + 36)
        .position(|window| {
            window[..5] == [56, 3, 37, 0x24, 0x00]
                && window[5..] == ability_uuid.as_bytes()[..]
        })?
        + 1;
    Some(ExecuteAbility {
        role_offset,
        ability_uuid,
    })
}

/// A decoded `EquipAbilitiesAndConsumables` (56): the client's declaration of the
/// consumable item it has equipped for this match, plus how many charges it has left.
#[derive(Debug, Clone, PartialEq)]
pub struct EquippedConsumable {
    /// The consumable ITEM uuid (e.g. "Potion of Light Healing").
    pub consumable_uuid: String,
    /// Remaining charges the client believes it holds (propId 5). Advisory: the
    /// server gates uses with `CONSUMABLES_PER_ROUND`, not with this number.
    pub charges: i64,
}

/// Decode an `EquipAbilitiesAndConsumables` (56) c2s frame.
///
/// Capture-derived layout (prod, carrier `0x36`, all Autonomous): `{0:Int avatarObj ·
/// 1:Byte 56 Avatar · 2:Byte 3 Autonomous · 3:Byte 56 · 4:String consumableUuid ·
/// 5:Int charges}` — e.g. s127 #954909 `… d826ea12-…-4de608281735, 6`, and the
/// charge count visibly decrements across a session (6, 6, 5, 5, 3, 3) as the client
/// re-uploads after each use.
///
/// This is the ONLY place the equipped consumable's UUID appears on the wire, so the
/// server must latch it here in order to answer a later `RequestConsumeConsumable`
/// (63) — which carries no payload of its own. Returns `None` for any other frame,
/// or when the declaration carries no consumable.
pub fn parse_equip_consumables(user_data: &[u8]) -> Option<EquippedConsumable> {
    if super::messages::user_message_gmid(user_data)
        != Some(arena_proto::GameMessageId::EquipAbilitiesAndConsumables as u8)
    {
        return None;
    }
    let nd = arena_proto::parse_netdata(user_data.get(2..)?);
    let uuid = nd.string(4)?.to_string();
    if uuid.is_empty() {
        return None;
    }
    Some(EquippedConsumable { consumable_uuid: uuid, charges: nd.int(5).unwrap_or(0) })
}

/// True iff a carrier-`0x36` c2s frame is a `RequestConsumeConsumable` (63) — the
/// client asking to drink its equipped potion.
///
/// Capture-derived: **269 prod frames, every one c2s and every one this exact bare
/// shape** — `{0:Int avatarObj · 1:Byte 56 Avatar · 2:Byte 3 Autonomous · 3:Byte 63}`,
/// i.e. NetObjectInfo + GameMessageId and nothing else (s127 #962747 =
/// `be36 030f 7077 3b020000 38 03 3f`). There is no item id in the request: the
/// server answers with `PerformConsumeConsumable` (64) carrying the UUID the same
/// avatar declared via op56.
pub fn is_request_consume_consumable(user_data: &[u8]) -> bool {
    super::messages::user_message_gmid(user_data)
        == Some(arena_proto::GameMessageId::RequestConsumeConsumable as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `arena-combat-reference.md` §op37 worked example (frame 954963):
    /// `be 36 04 1f 70 77 0a 35 02 00 00 38 03 25 24 00 <uuid>`.
    fn op37_frame() -> Vec<u8> {
        let mut v = vec![
            0xBE, 0x36, 0x04, 0x1F, 0x70, 0x77, 0x0A, 0x35, // marker+carrier + NetObjectInfo region
            0x02, 0x00, 0x00, // remaining LE bytes of p0 = 565
            0x38, // type (skip)
            0x03, // role = Autonomous (c2s)
            0x25, // gmid = 37
            0x24, 0x00, // len = 36
        ];
        v.extend_from_slice(b"7fc15804-1637-40a9-8dcc-3ea1eb0f778d");
        v
    }

    #[test]
    fn decodes_execute_ability() {
        let ea = parse_execute_ability(&op37_frame()).expect("op37 decodes");
        assert_eq!(ea.role_offset, 12);
        assert_eq!(ea.ability_uuid, "7fc15804-1637-40a9-8dcc-3ea1eb0f778d");
    }

    #[test]
    fn avatar_id_bytes_are_not_part_of_execute_ability_framing() {
        let mut frame = op37_frame();
        frame[7..11].copy_from_slice(&777_i32.to_le_bytes());
        let ea = parse_execute_ability(&frame).expect("an arbitrary Avatar id still parses");
        assert_eq!(ea.role_offset, 12);
    }

    /// Real prod c2s `EquipAbilitiesAndConsumables` (56), session 127 frame 954909.
    fn op56_frame() -> Vec<u8> {
        let mut v = vec![
            0xBE, 0x36, // marker + carrier 0x36
            0x05, // maxPropId = 5
            0x3F, // presence bitmap: props 0..=5
            0x70, 0x77, 0x0A, // type nibbles: Int, Byte, Byte, Byte, String, Int
            0x35, 0x02, 0x00, 0x00, // p0 = 565 (avatar net object)
            0x38, // p1 = 56 (Avatar)
            0x03, // p2 = 3 (Autonomous)
            0x38, // p3 = 56 (EquipAbilitiesAndConsumables)
            0x24, 0x00, // p4 length = 36
        ];
        v.extend_from_slice(b"d826ea12-e583-47c1-a50f-4de608281735");
        v.extend_from_slice(&[0x06, 0x00, 0x00, 0x00]); // p5 = 6 charges
        v
    }

    /// Real prod c2s `RequestConsumeConsumable` (63), session 127 frame 962747 —
    /// the WHOLE frame is 13 bytes: NetObjectInfo + gmid, no payload.
    fn op63_frame() -> Vec<u8> {
        vec![
            0xBE, 0x36, 0x03, 0x0F, 0x70, 0x77, 0x3B, 0x02, 0x00, 0x00, 0x38, 0x03, 0x3F,
        ]
    }

    #[test]
    fn decodes_equipped_consumable_from_capture() {
        let eq = parse_equip_consumables(&op56_frame()).expect("op56 decodes");
        assert_eq!(eq.consumable_uuid, "d826ea12-e583-47c1-a50f-4de608281735");
        assert_eq!(eq.charges, 6);
        // An op37/op63 frame is not an equip declaration.
        assert!(parse_equip_consumables(&op37_frame()).is_none());
        assert!(parse_equip_consumables(&op63_frame()).is_none());
    }

    #[test]
    fn detects_request_consume_consumable_from_capture() {
        assert!(is_request_consume_consumable(&op63_frame()));
        assert!(!is_request_consume_consumable(&op37_frame()));
        assert!(!is_request_consume_consumable(&op56_frame()));
        // The capture's op63 body really is bare: propIds 0..=3 only.
        let nd = arena_proto::parse_netdata(&op63_frame()[2..]);
        assert!(nd.ok);
        assert_eq!(nd.int(0), Some(571));
        assert_eq!(nd.int(1), Some(56));
        assert_eq!(nd.int(2), Some(3));
        assert_eq!(nd.int(3), Some(63));
        assert_eq!(nd.props.len(), 4, "op63 carries no item id");
    }

    #[test]
    fn swipe_input_is_not_an_ability() {
        // A short carrier-54 body (no NetData/gmid) is not an ability request.
        assert!(parse_execute_ability(&[0x84, 0x36]).is_none());
        assert!(parse_execute_ability(&[0xBE, 0x36, 0x03, 0x0F, 0x70, 0x77]).is_none());
    }
}
