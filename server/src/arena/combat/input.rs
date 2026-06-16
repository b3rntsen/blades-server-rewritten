//! Decoders for inbound c2s combat inputs (carrier MessageType `0x36` = 54).
//!
//! Carrier 54 is shared by the swipe-input, position, and ability-request messages
//! (and more); they're disambiguated structurally. This module decodes the ones
//! the engine acts on. Layouts: `docs/archive/arena-combat-reference.md`.

/// A decoded `RequestExecuteAbility` (37): the ability *instance* UUID being cast
/// and the offset of the `02 00 00` NetData separator (so the echo
/// `PerformExecuteAbility` can patch role+gmid in place).
#[derive(Debug, Clone, PartialEq)]
pub struct ExecuteAbility {
    pub sep_offset: usize,
    pub ability_uuid: String,
}

/// Detect + decode a `RequestExecuteAbility` (37) in a carrier-54 c2s `user_data`.
/// The body is separator-anchored (`arena-combat-reference.md` §op37):
///   `… 02 00 00 [type][role][gmid=37][u16-LE len=0x24][36-byte ASCII UUID]`.
/// Returns `None` if the frame isn't an ability request (e.g. a swipe input).
pub fn parse_execute_ability(user_data: &[u8]) -> Option<ExecuteAbility> {
    // Scan for the `02 00 00` separator whose gmid byte is 37 and is followed by a
    // 36-char UUID length. (Constrained enough to not false-match a swipe body.)
    let mut i = 2; // past marker + carrier
    while i + 8 + 36 <= user_data.len() {
        if user_data[i] == 0x02
            && user_data[i + 1] == 0x00
            && user_data[i + 2] == 0x00
            && user_data[i + 5] == 37 // gmid = RequestExecuteAbility
            && user_data[i + 6] == 0x24 // u16-LE length = 36 …
            && user_data[i + 7] == 0x00
        {
            let uuid = String::from_utf8_lossy(&user_data[i + 8..i + 8 + 36]).into_owned();
            return Some(ExecuteAbility { sep_offset: i, ability_uuid: uuid });
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `arena-combat-reference.md` §op37 worked example (frame 954963):
    /// `be 36 04 1f 70 77 0a 35 02 00 00 38 03 25 24 00 <uuid>`.
    fn op37_frame() -> Vec<u8> {
        let mut v = vec![
            0xBE, 0x36, 0x04, 0x1F, 0x70, 0x77, 0x0A, 0x35, // marker+carrier + NetObjectInfo region
            0x02, 0x00, 0x00, // separator @ offset 8
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
        assert_eq!(ea.sep_offset, 8);
        assert_eq!(ea.ability_uuid, "7fc15804-1637-40a9-8dcc-3ea1eb0f778d");
    }

    #[test]
    fn swipe_input_is_not_an_ability() {
        // A short carrier-54 body (no separator/gmid) is not an ability request.
        assert!(parse_execute_ability(&[0x84, 0x36]).is_none());
        assert!(parse_execute_ability(&[0xBE, 0x36, 0x03, 0x0F, 0x70, 0x77]).is_none());
    }
}
