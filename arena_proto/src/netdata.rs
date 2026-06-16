//! `NetData` property-stream codec — the typed property-bag serialization used
//! by Blades' arena game messages (`NetTransportMessage.SerializeNetData`).
//!
//! Symmetric counterpart to `parseNetData` in the capture platform's
//! `web/lib/arena-combat.ts`: [`parse_netdata`] is a byte-for-byte port of that
//! decoder, and [`NetDataWriter`] is its inverse — the encoder the arena server
//! needs to *build* authoritative s2c messages (`ReceiveDamage`,
//! `CombatScreenInfo`, status effects, …). Parse→encode round-trips to identical
//! bytes (see tests), so the server emits exactly what the retail client expects.
//!
//! Wire layout (`docs/archive/arena-combat-reference.md` §"The NetData property
//! stream"), all relative to the start of a message *body* (i.e. after the
//! `marker` + `MessageType` bytes for NetData-framed opcodes):
//!
//! ```text
//! [maxPropId : u8]
//! [presence bitmap : (maxPropId>>3)+1 bytes, LSB-first — bit p set ⇒ propId p present]
//! [type nibbles : ceil(nProps/2) bytes — one NetDataType (4 bits) per present
//!                 propId, low-nibble = even index then high-nibble = odd, ascending]
//! [values : ascending propId order]
//! ```
//!
//! Scalars are little-endian; `String`/`ByteArray` carry a u16-LE length prefix.
//! UUIDs are `String`s of length 0x24 (36 ASCII chars). `Vector2`/`Vector3` are
//! kept as raw bytes (the decoded combat opcodes don't interpret them).

use std::collections::BTreeMap;

/// `NetDataType` tag (the 4-bit type nibble). Values match the il2cpp enum and
/// the `NETDATA_WIDTH` table in `arena-combat.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NetDataType {
    Int = 0,
    UInt = 1,
    ULong = 2,
    Long = 3,
    Double = 4,
    Float = 5,
    Bool = 6,
    Byte = 7,
    Int16 = 8,
    UInt16 = 9,
    String = 10,
    Vector2 = 11,
    Vector3 = 12,
    ByteArray = 13,
    NetData = 14,
    None = 15,
}

impl NetDataType {
    pub fn from_nibble(n: u8) -> Option<Self> {
        use NetDataType::*;
        Some(match n & 0x0f {
            0 => Int,
            1 => UInt,
            2 => ULong,
            3 => Long,
            4 => Double,
            5 => Float,
            6 => Bool,
            7 => Byte,
            8 => Int16,
            9 => UInt16,
            10 => String,
            11 => Vector2,
            12 => Vector3,
            13 => ByteArray,
            14 => NetData,
            15 => None,
            _ => unreachable!(),
        })
    }

    /// Fixed value width in bytes, or `None` for variable-length / nested types
    /// (`String`, `ByteArray`, `NetData`) whose length is read from the stream.
    pub fn fixed_width(self) -> Option<usize> {
        use NetDataType::*;
        Some(match self {
            Int | UInt | Float => 4,
            ULong | Long | Double | Vector2 => 8,
            Bool | Byte => 1,
            Int16 | UInt16 => 2,
            Vector3 => 12,
            None => 0,
            String | ByteArray | NetData => return Option::None,
        })
    }
}

/// A decoded NetData property value.
#[derive(Debug, Clone, PartialEq)]
pub enum NetDataValue {
    Int(i32),
    UInt(u32),
    ULong(u64),
    Long(i64),
    Double(f64),
    Float(f32),
    Bool(bool),
    Byte(u8),
    Int16(i16),
    UInt16(u16),
    String(String),
    /// Raw 8 bytes (two f32 LE in practice).
    Vector2([u8; 8]),
    /// Raw 12 bytes (three f32 LE in practice).
    Vector3([u8; 12]),
    ByteArray(Vec<u8>),
}

impl NetDataValue {
    pub fn type_tag(&self) -> NetDataType {
        use NetDataValue as V;
        match self {
            V::Int(_) => NetDataType::Int,
            V::UInt(_) => NetDataType::UInt,
            V::ULong(_) => NetDataType::ULong,
            V::Long(_) => NetDataType::Long,
            V::Double(_) => NetDataType::Double,
            V::Float(_) => NetDataType::Float,
            V::Bool(_) => NetDataType::Bool,
            V::Byte(_) => NetDataType::Byte,
            V::Int16(_) => NetDataType::Int16,
            V::UInt16(_) => NetDataType::UInt16,
            V::String(_) => NetDataType::String,
            V::Vector2(_) => NetDataType::Vector2,
            V::Vector3(_) => NetDataType::Vector3,
            V::ByteArray(_) => NetDataType::ByteArray,
        }
    }

    /// Convenience: read this value as an integer if it is one of the integral
    /// types (handy for propIds like netObjectId / gameMessageId).
    pub fn as_i64(&self) -> Option<i64> {
        use NetDataValue as V;
        Some(match self {
            V::Int(v) => *v as i64,
            V::UInt(v) => *v as i64,
            V::ULong(v) => *v as i64,
            V::Long(v) => *v,
            V::Byte(v) => *v as i64,
            V::Int16(v) => *v as i64,
            V::UInt16(v) => *v as i64,
            V::Bool(v) => *v as i64,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            NetDataValue::String(s) => Some(s),
            _ => None,
        }
    }
}

/// Result of [`parse_netdata`]: the decoded properties keyed by propId, plus the
/// byte cursor and an `ok` flag (false ⇒ the stream ran out of bytes mid-value,
/// exactly like the TS decoder's early-return contract).
#[derive(Debug, Clone, PartialEq)]
pub struct NetDataParse {
    /// propId → decoded value (ascending; `BTreeMap` keeps order deterministic).
    pub props: BTreeMap<u8, NetDataValue>,
    /// Bytes consumed from `body`.
    pub consumed: usize,
    /// True iff the stream parsed without running out of bytes.
    pub ok: bool,
}

impl NetDataParse {
    pub fn get(&self, prop_id: u8) -> Option<&NetDataValue> {
        self.props.get(&prop_id)
    }
    pub fn int(&self, prop_id: u8) -> Option<i64> {
        self.props.get(&prop_id).and_then(NetDataValue::as_i64)
    }
    pub fn string(&self, prop_id: u8) -> Option<&str> {
        self.props.get(&prop_id).and_then(NetDataValue::as_str)
    }
}

#[inline]
fn le_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

/// Decode a NetData property stream from the start of `body`. Faithful port of
/// `parseNetData` (`arena-combat.ts`): on truncation it returns what was decoded
/// so far with `ok = false` rather than erroring.
pub fn parse_netdata(body: &[u8]) -> NetDataParse {
    let mut props = BTreeMap::new();
    if body.is_empty() {
        return NetDataParse { props, consumed: 0, ok: false };
    }

    let mut i = 0usize;
    let max_prop_id = body[i] as usize;
    i += 1;

    let bm_len = (max_prop_id >> 3) + 1;
    if i + bm_len > body.len() {
        return NetDataParse { props, consumed: i, ok: false };
    }
    // Present propIds, ascending (LSB-first within each bitmap byte).
    let mut prop_ids: Vec<u8> = Vec::new();
    for n in 0..bm_len {
        let by = body[i + n];
        for k in 0..8 {
            if by & (1 << k) != 0 {
                prop_ids.push((n * 8 + k) as u8);
            }
        }
    }
    i += bm_len;

    let n_type_bytes = (prop_ids.len() + 1) >> 1;
    if i + n_type_bytes > body.len() {
        return NetDataParse { props, consumed: i, ok: false };
    }
    // Type nibble per present propId: even index → low nibble, odd → high nibble.
    let mut types: Vec<NetDataType> = Vec::with_capacity(prop_ids.len());
    for idx in 0..prop_ids.len() {
        let byte = body[i + (idx >> 1)];
        let nib = if idx % 2 == 0 { byte & 0x0f } else { byte >> 4 };
        types.push(NetDataType::from_nibble(nib).expect("nibble is 4 bits"));
    }
    i += n_type_bytes;

    for (p, &pid) in prop_ids.iter().enumerate() {
        let ty = types[p];
        match ty {
            NetDataType::String | NetDataType::ByteArray => {
                if i + 2 > body.len() {
                    return NetDataParse { props, consumed: i, ok: false };
                }
                let l = le_u16(body, i) as usize;
                i += 2;
                if i + l > body.len() {
                    return NetDataParse { props, consumed: i, ok: false };
                }
                let val = if ty == NetDataType::String {
                    NetDataValue::String(String::from_utf8_lossy(&body[i..i + l]).into_owned())
                } else {
                    NetDataValue::ByteArray(body[i..i + l].to_vec())
                };
                props.insert(pid, val);
                i += l;
            }
            _ => {
                let w = match ty.fixed_width() {
                    Some(w) => w,
                    None => {
                        // NetData (nested) — not produced by the decoded opcodes;
                        // bail rather than guess a length.
                        return NetDataParse { props, consumed: i, ok: false };
                    }
                };
                if i + w > body.len() {
                    return NetDataParse { props, consumed: i, ok: false };
                }
                let s = &body[i..i + w];
                let val = match ty {
                    NetDataType::Int => NetDataValue::Int(i32::from_le_bytes(s.try_into().unwrap())),
                    NetDataType::UInt => {
                        NetDataValue::UInt(u32::from_le_bytes(s.try_into().unwrap()))
                    }
                    NetDataType::ULong => {
                        NetDataValue::ULong(u64::from_le_bytes(s.try_into().unwrap()))
                    }
                    NetDataType::Long => {
                        NetDataValue::Long(i64::from_le_bytes(s.try_into().unwrap()))
                    }
                    NetDataType::Double => {
                        NetDataValue::Double(f64::from_le_bytes(s.try_into().unwrap()))
                    }
                    NetDataType::Float => {
                        NetDataValue::Float(f32::from_le_bytes(s.try_into().unwrap()))
                    }
                    NetDataType::Bool => NetDataValue::Bool(s[0] != 0),
                    NetDataType::Byte => NetDataValue::Byte(s[0]),
                    NetDataType::Int16 => {
                        NetDataValue::Int16(i16::from_le_bytes(s.try_into().unwrap()))
                    }
                    NetDataType::UInt16 => {
                        NetDataValue::UInt16(u16::from_le_bytes(s.try_into().unwrap()))
                    }
                    NetDataType::Vector2 => NetDataValue::Vector2(s.try_into().unwrap()),
                    NetDataType::Vector3 => NetDataValue::Vector3(s.try_into().unwrap()),
                    NetDataType::None => {
                        // zero-width; represent as Byte(0) sentinel is wrong — skip.
                        i += w;
                        continue;
                    }
                    NetDataType::String | NetDataType::ByteArray | NetDataType::NetData => {
                        unreachable!("handled above")
                    }
                };
                props.insert(pid, val);
                i += w;
            }
        }
    }

    NetDataParse { props, consumed: i, ok: true }
}

/// Builder for a NetData property stream. Accumulate `(propId, value)` pairs in
/// any order, then [`finish`](Self::finish) emits the canonical bytes (maxPropId
/// + presence bitmap + type nibbles + ascending values) — the exact inverse of
/// [`parse_netdata`].
#[derive(Debug, Default, Clone)]
pub struct NetDataWriter {
    props: BTreeMap<u8, NetDataValue>,
}

impl NetDataWriter {
    pub fn new() -> Self {
        Self { props: BTreeMap::new() }
    }

    /// Set a property (replaces any previous value at `prop_id`). Chainable.
    pub fn put(&mut self, prop_id: u8, value: NetDataValue) -> &mut Self {
        self.props.insert(prop_id, value);
        self
    }

    // --- typed convenience setters -----------------------------------------
    pub fn int(&mut self, p: u8, v: i32) -> &mut Self {
        self.put(p, NetDataValue::Int(v))
    }
    pub fn uint(&mut self, p: u8, v: u32) -> &mut Self {
        self.put(p, NetDataValue::UInt(v))
    }
    pub fn ulong(&mut self, p: u8, v: u64) -> &mut Self {
        self.put(p, NetDataValue::ULong(v))
    }
    pub fn float(&mut self, p: u8, v: f32) -> &mut Self {
        self.put(p, NetDataValue::Float(v))
    }
    pub fn bool(&mut self, p: u8, v: bool) -> &mut Self {
        self.put(p, NetDataValue::Bool(v))
    }
    pub fn byte(&mut self, p: u8, v: u8) -> &mut Self {
        self.put(p, NetDataValue::Byte(v))
    }
    pub fn int16(&mut self, p: u8, v: i16) -> &mut Self {
        self.put(p, NetDataValue::Int16(v))
    }
    /// Write a `String` value (UUIDs go here too — pass the lowercase
    /// hyphenated 36-char form; the wire length prefix `0x24` is implied).
    pub fn string(&mut self, p: u8, v: impl Into<String>) -> &mut Self {
        self.put(p, NetDataValue::String(v.into()))
    }

    /// Helper: write the actor `NetObjectInfo` at the canonical propIds 0/1/2
    /// (`netObjectId` Int, `netObjectType` Byte, `netRole` Byte).
    pub fn net_object_info(&mut self, net_object_id: i32, net_object_type: u8, net_role: u8) -> &mut Self {
        self.int(0, net_object_id)
            .byte(1, net_object_type)
            .byte(2, net_role)
    }

    /// Serialize to the canonical NetData byte layout.
    pub fn finish(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let max_prop_id = self.props.keys().copied().max().unwrap_or(0);
        out.push(max_prop_id);

        // Presence bitmap, LSB-first.
        let bm_len = (max_prop_id as usize >> 3) + 1;
        let mut bitmap = vec![0u8; bm_len];
        for &pid in self.props.keys() {
            bitmap[pid as usize >> 3] |= 1 << (pid as usize & 7);
        }
        out.extend_from_slice(&bitmap);

        // Type nibbles, ascending propId: even index → low nibble, odd → high.
        let ids: Vec<u8> = self.props.keys().copied().collect();
        let n_type_bytes = (ids.len() + 1) >> 1;
        let mut type_bytes = vec![0u8; n_type_bytes];
        for (idx, &pid) in ids.iter().enumerate() {
            let tag = self.props[&pid].type_tag() as u8;
            if idx % 2 == 0 {
                type_bytes[idx >> 1] |= tag & 0x0f;
            } else {
                type_bytes[idx >> 1] |= (tag & 0x0f) << 4;
            }
        }
        out.extend_from_slice(&type_bytes);

        // Values, ascending propId.
        for (_pid, v) in &self.props {
            encode_value(&mut out, v);
        }
        out
    }
}

fn encode_value(out: &mut Vec<u8>, v: &NetDataValue) {
    use NetDataValue as V;
    match v {
        V::Int(x) => out.extend_from_slice(&x.to_le_bytes()),
        V::UInt(x) => out.extend_from_slice(&x.to_le_bytes()),
        V::ULong(x) => out.extend_from_slice(&x.to_le_bytes()),
        V::Long(x) => out.extend_from_slice(&x.to_le_bytes()),
        V::Double(x) => out.extend_from_slice(&x.to_le_bytes()),
        V::Float(x) => out.extend_from_slice(&x.to_le_bytes()),
        V::Bool(x) => out.push(*x as u8),
        V::Byte(x) => out.push(*x),
        V::Int16(x) => out.extend_from_slice(&x.to_le_bytes()),
        V::UInt16(x) => out.extend_from_slice(&x.to_le_bytes()),
        V::String(s) => {
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        V::ByteArray(b) => {
            out.extend_from_slice(&(b.len() as u16).to_le_bytes());
            out.extend_from_slice(b);
        }
        V::Vector2(b) => out.extend_from_slice(b),
        V::Vector3(b) => out.extend_from_slice(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// op55 CombatScreenInfo body captured in session 293 (frame 1955386, after
    /// the `BE 37` marker+opcode): NetObjectInfo {id=437, type=55, role=2}.
    const OP55_BODY: &[u8] = &[0x02, 0x07, 0x70, 0x07, 0xB5, 0x01, 0x00, 0x00, 0x37, 0x02];

    #[test]
    fn parse_op55_netobjectinfo() {
        let p = parse_netdata(OP55_BODY);
        assert!(p.ok);
        assert_eq!(p.consumed, OP55_BODY.len());
        assert_eq!(p.get(0), Some(&NetDataValue::Int(437)));
        assert_eq!(p.get(1), Some(&NetDataValue::Byte(55)));
        assert_eq!(p.get(2), Some(&NetDataValue::Byte(2)));
        assert_eq!(p.props.len(), 3);
    }

    #[test]
    fn roundtrip_op55() {
        let p = parse_netdata(OP55_BODY);
        let mut w = NetDataWriter::new();
        for (pid, v) in &p.props {
            w.put(*pid, v.clone());
        }
        assert_eq!(w.finish(), OP55_BODY, "encode∘decode must be identity");
    }

    #[test]
    fn encode_netobjectinfo_from_values_matches_reference() {
        // arena-combat-reference.md op55 worked example: id=561, type=55, role=3
        // → `02 07 7007 31020000 37 03`.
        let mut w = NetDataWriter::new();
        w.net_object_info(561, 55, 3);
        assert_eq!(
            w.finish(),
            &[0x02, 0x07, 0x70, 0x07, 0x31, 0x02, 0x00, 0x00, 0x37, 0x03]
        );
    }

    /// A sparse stream with a String value — propIds {0,1,2,4}, captured in
    /// session 293 (frame 1955417, after `BE 32`): the propId4 UUID exercises
    /// the gap in the bitmap + the u16-LE length-prefixed String path.
    fn op50ish_body() -> Vec<u8> {
        let mut b = vec![
            0x04, 0x17, 0x70, 0xA7, // maxPropId=4, bitmap {0,1,2,4}, types [Int,Byte,Byte,String]
            0xB9, 0x01, 0x00, 0x00, // propId0 Int = 441
            0x38, // propId1 Byte = 56
            0x03, // propId2 Byte = 3
            0x24, 0x00, // propId4 String len = 36
        ];
        b.extend_from_slice(b"30074991-417c-45e6-a73a-ace52b659338");
        b
    }

    #[test]
    fn parse_sparse_with_string() {
        let body = op50ish_body();
        let p = parse_netdata(&body);
        assert!(p.ok);
        assert_eq!(p.consumed, body.len());
        assert_eq!(p.get(0), Some(&NetDataValue::Int(441)));
        assert_eq!(p.get(1), Some(&NetDataValue::Byte(56)));
        assert_eq!(p.get(2), Some(&NetDataValue::Byte(3)));
        assert_eq!(
            p.string(4),
            Some("30074991-417c-45e6-a73a-ace52b659338")
        );
        assert!(p.get(3).is_none(), "propId 3 absent (gap in bitmap)");
    }

    #[test]
    fn roundtrip_sparse_with_string() {
        let body = op50ish_body();
        let p = parse_netdata(&body);
        let mut w = NetDataWriter::new();
        for (pid, v) in &p.props {
            w.put(*pid, v.clone());
        }
        assert_eq!(w.finish(), body);
    }

    #[test]
    fn truncated_stream_reports_not_ok() {
        // maxPropId=4, bitmap claims {0,1,2,4}, but no type/value bytes follow.
        let p = parse_netdata(&[0x04, 0x17]);
        assert!(!p.ok);
    }

    #[test]
    fn packed_stats_roundtrip() {
        // ReceiveDamage propId 4/5 pack Health|Stamina<<10|Magicka<<20 with a
        // sequenceId in the hi32. Prove the ULong path is exact.
        let health = 812u64;
        let stamina = 640u64;
        let magicka = 300u64;
        let seq = 627_048_447u64;
        let packed = (health | (stamina << 10) | (magicka << 20)) | (seq << 32);
        let mut w = NetDataWriter::new();
        w.ulong(4, packed);
        let bytes = w.finish();
        let p = parse_netdata(&bytes);
        assert_eq!(p.get(4), Some(&NetDataValue::ULong(packed)));
        // unpack back
        if let Some(NetDataValue::ULong(v)) = p.get(4) {
            assert_eq!(v & 0x3ff, health);
            assert_eq!((v >> 10) & 0x3ff, stamina);
            assert_eq!((v >> 20) & 0x3ff, magicka);
            assert_eq!(v >> 32, seq);
        }
    }
}
