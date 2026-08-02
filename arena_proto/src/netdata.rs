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
//! Scalars are little-endian. The length prefix of a variable-length value is
//! **per type, not uniform** — see [`NetDataType::len_prefix_width`]:
//! `String` = u16-LE, `ByteArray` = **u8**. UUIDs are `String`s of length 0x24
//! (36 ASCII chars). `Vector2`/`Vector3` are kept as raw bytes (the decoded
//! combat opcodes don't interpret them).
//!
//! # Where the widths come from (2026-07-25 audit)
//!
//! Not from the code's assumptions: from 288,414 real captured `UserMessage`
//! bodies on prod (every `decrypt_status='ok'` frame with a resolved
//! `game_message_id`). Each body was parsed under all four `String` × `ByteArray`
//! width combinations and scored on whether the parse consumes the body
//! **exactly** — the only self-consistent outcome for a correctly-sized decoder.
//!
//! | String | ByteArray | bodies consumed exactly |
//! |---|---|---|
//! | u16 | **u8** | **286,988 (99.51 %)** |
//! | u8 | u8 | 247,541 (85.83 %) |
//! | u16 | u16 (the old code) | 213,100 (73.89 %) |
//! | u8 | u16 | 179,203 (62.13 %) |
//!
//! Isolating bodies that carry exactly one variable-length value makes it
//! unambiguous: 35,299 `String`-only bodies fit at u16 and **zero** at u8;
//! 68,344 `ByteArray`-only bodies fit at u8 and **zero** at u16. Every one of
//! the 5,550 bodies carrying *both* types fits only as (`String` u16,
//! `ByteArray` u8). The 1,426 residual non-fits under the winning combination
//! are 1,410 fragment-0-only bodies (a truncated first slice of a fragmented
//! message — structurally unable to fit) and 16 malformed / non-NetData
//! carriers, none of them width-related.
//!
//! Consequence: with the old uniform-u16 rule `parse_netdata` could not
//! round-trip a retail op53 `PlayerChannelingStateChange` at all — all 2,450
//! prod op53 frames fit at ByteArray=u8 and none at u16. Corpus-wide the widest
//! real `ByteArray` value is 23 B (the widest `String` is a 36 B UUID), i.e. the
//! u8 prefix is not merely consistent, it is comfortable.
//!
//! `NetData` (nested, type 14) never occurs in the corpus, so no width is
//! claimed for it; both the parser and the writer refuse to guess.

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

    /// Width in bytes of this type's little-endian length prefix, for the
    /// variable-length types only. **The width differs by type** — see the
    /// module docs for the corpus evidence:
    ///
    /// * `String` → 2 (u16-LE)
    /// * `ByteArray` → 1 (u8)
    /// * everything else (including nested `NetData`) → `None`
    ///
    /// Getting this uniform was the 2026-07-25 bug: writing a u16 prefix for a
    /// `ByteArray` desynchronises the whole property stream for a retail client,
    /// because every following propId is then read from inside the payload.
    pub fn len_prefix_width(self) -> Option<usize> {
        match self {
            NetDataType::String => Some(2),
            NetDataType::ByteArray => Some(1),
            _ => Option::None,
        }
    }

    /// The largest value this type's length prefix can express, for the
    /// variable-length types (`String` 65 535 B, `ByteArray` 255 B).
    pub fn max_value_len(self) -> Option<usize> {
        self.len_prefix_width().map(|w| (1usize << (8 * w)) - 1)
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

/// Read a little-endian length prefix of `width` bytes (1 or 2).
#[inline]
fn le_len(b: &[u8], o: usize, width: usize) -> usize {
    match width {
        1 => b[o] as usize,
        _ => u16::from_le_bytes([b[o], b[o + 1]]) as usize,
    }
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
            // Length-prefixed: the prefix WIDTH depends on the type (String u16,
            // ByteArray u8 — module docs).
            NetDataType::String | NetDataType::ByteArray => {
                let lw = ty.len_prefix_width().expect("String/ByteArray are prefixed");
                if i + lw > body.len() {
                    return NetDataParse { props, consumed: i, ok: false };
                }
                let l = le_len(body, i, lw);
                i += lw;
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

/// The largest byte length a NetData `String` value can carry: its wire length
/// prefix is a **u16** (`[len: u16-LE][bytes]`), so 65 535 bytes is the hard
/// ceiling. Anything longer cannot be represented — writing `len as u16`
/// silently WRAPS, which desynchronises the whole property stream for the reader
/// (every subsequent propId is decoded from the middle of the oversize payload).
/// [`NetDataWriter::finish`] therefore truncates deliberately and logs; use
/// [`NetDataWriter::finish_checked`] to get an explicit error instead.
///
/// Latent today: the largest real payload is the op54 PROFILE character JSON
/// (~20 KB in retail s506), well under the ceiling. It becomes reachable the
/// moment a character JSON grows past 64 KB.
pub const NETDATA_MAX_STRING_LEN: usize = u16::MAX as usize;

/// The largest byte length a NetData `ByteArray` value can carry: its wire length
/// prefix is a single **u8** (module docs), so 255 bytes is the hard ceiling —
/// 257× tighter than a `String`'s, which is exactly why the width has to be
/// modelled per type rather than assumed uniform.
///
/// Not latent, but not tight either: the widest `ByteArray` in the entire
/// captured corpus is 23 B (the op53 channeling state blob).
pub const NETDATA_MAX_BYTEARRAY_LEN: usize = u8::MAX as usize;

/// A `String`/`ByteArray` property too long for **its own** wire length prefix.
/// Returned by [`NetDataWriter::finish_checked`] / [`NetDataWriter::overflows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetDataOverflow {
    /// The propId whose value is oversize.
    pub prop_id: u8,
    /// The value's true byte length (> `limit`).
    pub len: usize,
    /// The ceiling this value's *type* imposes: [`NETDATA_MAX_STRING_LEN`] for a
    /// `String`, [`NETDATA_MAX_BYTEARRAY_LEN`] for a `ByteArray`.
    pub limit: usize,
}

impl std::fmt::Display for NetDataOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "NetData propId {} value is {} B, over its length-prefix ceiling of {} B",
            self.prop_id, self.len, self.limit
        )
    }
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
    pub fn long(&mut self, p: u8, v: i64) -> &mut Self {
        self.put(p, NetDataValue::Long(v))
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

    /// Every `String`/`ByteArray` property whose value exceeds **its own type's**
    /// wire length prefix ([`NETDATA_MAX_STRING_LEN`] / [`NETDATA_MAX_BYTEARRAY_LEN`]).
    /// Empty for every normal payload.
    pub fn overflows(&self) -> Vec<NetDataOverflow> {
        self.props
            .iter()
            .filter_map(|(&prop_id, v)| {
                let (len, limit) = match v {
                    NetDataValue::String(s) => (s.len(), NETDATA_MAX_STRING_LEN),
                    NetDataValue::ByteArray(b) => (b.len(), NETDATA_MAX_BYTEARRAY_LEN),
                    _ => return None,
                };
                (len > limit).then_some(NetDataOverflow { prop_id, len, limit })
            })
            .collect()
    }

    /// Like [`finish`](Self::finish), but returns an explicit **error** instead of
    /// truncating when a `String`/`ByteArray` value can't fit its length
    /// prefix. Callers that must not ship a lossy frame should use this.
    pub fn finish_checked(&self) -> Result<Vec<u8>, Vec<NetDataOverflow>> {
        let over = self.overflows();
        if over.is_empty() { Ok(self.finish()) } else { Err(over) }
    }

    /// Serialize to the canonical NetData byte layout.
    ///
    /// A `String`/`ByteArray` longer than its type's ceiling
    /// ([`NETDATA_MAX_STRING_LEN`] / [`NETDATA_MAX_BYTEARRAY_LEN`]) is **truncated
    /// deliberately** (to the last whole UTF-8 code point for a `String`) and logged
    /// at `error!` — never silently wrapped, which would corrupt every property
    /// after it. [`finish_checked`](Self::finish_checked) surfaces the same
    /// condition as an error. Byte-identical to the old behaviour for every value
    /// at or under the ceiling.
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
        for (&pid, v) in &self.props {
            encode_value(&mut out, pid, v);
        }
        out
    }
}

/// Write a length-prefixed value, **checked**. `width` is the prefix width in
/// bytes for this value's type (`String` 2, `ByteArray` 1 — see
/// [`NetDataType::len_prefix_width`]); the inverse of the read in
/// [`parse_netdata`]. Over the ceiling we log loudly and truncate deliberately
/// (at a UTF-8 code-point boundary when `is_utf8`) rather than let the length
/// wrap — a wrapped prefix makes the reader resume parsing inside the payload
/// and mis-decode every following property.
fn write_len_prefixed(out: &mut Vec<u8>, prop_id: u8, bytes: &[u8], is_utf8: bool, width: usize) {
    let limit = (1usize << (8 * width)) - 1;
    let keep = if bytes.len() <= limit {
        bytes.len()
    } else {
        let mut keep = limit;
        if is_utf8 {
            // Back off to the last whole code point so the truncated value is
            // still valid UTF-8 (`str::floor_char_boundary` is unstable).
            while keep > 0 && (bytes[keep] & 0xC0) == 0x80 {
                keep -= 1;
            }
        }
        log::error!(
            "arena_proto::netdata: propId {prop_id} value is {} B — over its \
             {}-bit length-prefix ceiling of {limit} B. TRUNCATING to {keep} B \
             (the frame is lossy but still parseable). Use NetDataWriter::finish_checked \
             to reject instead.",
            bytes.len(),
            width * 8,
        );
        keep
    };
    match width {
        1 => out.push(keep as u8),
        _ => out.extend_from_slice(&(keep as u16).to_le_bytes()),
    }
    out.extend_from_slice(&bytes[..keep]);
}

fn encode_value(out: &mut Vec<u8>, prop_id: u8, v: &NetDataValue) {
    use NetDataValue as V;
    // The prefix width comes from the value's own type tag, so the writer can
    // never drift from `parse_netdata` / `NetDataType::len_prefix_width`.
    let prefix_width = |v: &NetDataValue| {
        v.type_tag()
            .len_prefix_width()
            .expect("only length-prefixed types reach here")
    };
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
        V::String(s) => write_len_prefixed(out, prop_id, s.as_bytes(), true, prefix_width(v)),
        V::ByteArray(b) => write_len_prefixed(out, prop_id, b, false, prefix_width(v)),
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

    // -----------------------------------------------------------------------
    // REAL retail frames. These are the evidence for the per-type length-prefix
    // widths (module docs): a `ByteArray` carries a u8 prefix while a `String`
    // in the SAME body carries a u16 one. Both bodies are verbatim prod capture
    // bytes, so a regression here means we can no longer speak to a retail client.
    // -----------------------------------------------------------------------

    /// op53 `PlayerChannelingStateChange`, prod frame 954966 (`game_message_id`
    /// 53, carrier `0x36`), the whole NetData body after the `BE 36` marker+type.
    /// `{0:Int 565 · 1:Byte 56 Avatar · 2:Byte 1 Authority · 3:Byte 53 · 4/5:ULong
    /// packed stats · 6:Byte 4 · 7:ByteArray(7) · 8:Float · 9:String uuid}`.
    fn op53_body() -> Vec<u8> {
        let mut b = hex(concat!(
            "09ff03707722d7a5",                     // maxPropId 9, bitmap {0..9}, 5 type bytes
            "35020000", "38", "01", "35",           // pid0 Int, pid1/2/3 Byte
            "1f000000f4216d25", "1f000000ffffff3f", // pid4/5 ULong
            "04",                                   // pid6 Byte
            "07", "040000001c0004",                 // pid7 ByteArray: u8 len 7 + 7 bytes
            "83d4ac3f",                             // pid8 Float
            "2400",                                 // pid9 String: u16 len 36
        ));
        b.extend_from_slice(b"7fc15804-1637-40a9-8dcc-3ea1eb0f778d");
        b
    }

    /// op58 `PlayerAbilityStateChange`, prod frame 961507 — same shape plus a
    /// trailing `10:Byte`, and a 13-byte `ByteArray` at propId 7.
    fn op58_body() -> Vec<u8> {
        let mut b = hex(concat!(
            "0aff07707722d7a507", // maxPropId 10, bitmap {0..10}, 6 type bytes
            "3b020000", "38", "01", "3a",
            "66000000ea92943d", "66000000ffffbf39",
            "0b",
            "0d", "0a0000001c000100040001000b",
            "a7bf7b3f",
            "2400",
        ));
        b.extend_from_slice(b"eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13");
        b.push(0x05);
        b
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    #[test]
    fn parse_real_op53_bytearray_uses_a_u8_length_prefix() {
        let body = op53_body();
        let p = parse_netdata(&body);
        assert!(p.ok, "a retail op53 body must parse");
        assert_eq!(
            p.consumed,
            body.len(),
            "the parse must consume the body EXACTLY — under the old uniform-u16 rule \
             it does not, which is how the bug was found",
        );
        assert_eq!(p.int(0), Some(565));
        assert_eq!(p.int(1), Some(56), "NetObjectType::Avatar");
        assert_eq!(p.int(2), Some(1), "NetRole::Authority");
        assert_eq!(p.int(3), Some(53), "GameMessageId 53");
        assert_eq!(p.get(4), Some(&NetDataValue::ULong(0x256d_21f4_0000_001f)));
        assert_eq!(p.get(5), Some(&NetDataValue::ULong(0x3fff_ffff_0000_001f)));
        assert_eq!(p.int(6), Some(4));
        assert_eq!(
            p.get(7),
            Some(&NetDataValue::ByteArray(hex("040000001c0004"))),
            "the state blob is 7 bytes behind a ONE-byte length prefix",
        );
        assert_eq!(p.get(8), Some(&NetDataValue::Float(1.3502353)));
        assert_eq!(p.string(9), Some("7fc15804-1637-40a9-8dcc-3ea1eb0f778d"));
    }

    /// The whole point: `parse → encode` reproduces a retail frame byte-for-byte,
    /// so the server can build one. This is what a uniform-u16 writer could not do.
    #[test]
    fn roundtrip_real_retail_bodies() {
        for (name, body) in [("op53", op53_body()), ("op58", op58_body())] {
            let p = parse_netdata(&body);
            assert!(p.ok && p.consumed == body.len(), "{name}: parses exactly");
            let mut w = NetDataWriter::new();
            for (pid, v) in &p.props {
                w.put(*pid, v.clone());
            }
            assert_eq!(w.finish(), body, "{name}: encode∘decode must be identity");
        }
    }

    /// Regression pin for the actual defect. Splicing a `00` high byte after the
    /// propId-7 length — precisely what the old u16 `ByteArray` writer emitted for
    /// these same properties — produces a body a retail client cannot read: the
    /// stream desynchronises by one byte and every property after the blob is
    /// decoded from the wrong offset.
    #[test]
    fn a_u16_bytearray_prefix_desynchronises_the_rest_of_the_stream() {
        const PROP7_LEN_OFFSET: usize = 32; // 8 hdr + 4 + 1 + 1 + 1 + 8 + 8 + 1
        let good = op53_body();
        assert_eq!(good[PROP7_LEN_OFFSET], 7, "test bug: not the length byte");

        let mut old_style = good.clone();
        old_style.insert(PROP7_LEN_OFFSET + 1, 0x00); // u8 len -> u16 len

        let p = parse_netdata(&old_style);
        // Everything up to the blob still reads (the desync starts at propId 7).
        assert_eq!(p.int(3), Some(53));
        assert_ne!(
            p.get(7),
            Some(&NetDataValue::ByteArray(hex("040000001c0004"))),
            "the blob must NOT survive a wrongly-widened prefix",
        );
        assert_ne!(
            p.string(9),
            Some("7fc15804-1637-40a9-8dcc-3ea1eb0f778d"),
            "the UUID after the blob must be mis-decoded — this is the corruption",
        );

        // And the current writer never produces that shape.
        let mut w = NetDataWriter::new();
        for (pid, v) in &parse_netdata(&good).props {
            w.put(*pid, v.clone());
        }
        assert_eq!(w.finish(), good);
        assert_ne!(w.finish(), old_style);
    }

    #[test]
    fn packed_stats_roundtrip() {
        // ReceiveDamage propId 4/5 pack Magicka|Stamina<<10|Health<<20 into the stat
        // word, with the sequenceId in the OTHER half. (Both the field order and the
        // half-split were once believed the other way round; see
        // `combat::state::PackedStats` for the capture evidence.) This test only
        // proves the ULong writer/parser path is exact, so it packs a value and reads
        // the same bits back — but it should not restate a layout that was wrong.
        let magicka = 812u64;
        let stamina = 640u64;
        let health = 300u64;
        let seq = 627_048_447u64;
        let packed = (magicka | (stamina << 10) | (health << 20)) | (seq << 32);
        let mut w = NetDataWriter::new();
        w.ulong(4, packed);
        let bytes = w.finish();
        let p = parse_netdata(&bytes);
        assert_eq!(p.get(4), Some(&NetDataValue::ULong(packed)));
        // unpack back
        if let Some(NetDataValue::ULong(v)) = p.get(4) {
            assert_eq!(v & 0x3ff, magicka);
            assert_eq!((v >> 10) & 0x3ff, stamina);
            assert_eq!((v >> 20) & 0x3ff, health);
            assert_eq!(v >> 32, seq);
        }
    }

    // -----------------------------------------------------------------------
    // Length-prefix overflow guard.
    //
    // The wire length prefix is a u16 for String and a u8 for ByteArray. Writing
    // the length un-checked SILENTLY WRAPS above the ceiling — e.g. a 70 000 B
    // string writes prefix 4 464, so the reader resumes parsing 65 536 bytes
    // early, *inside* the payload, and mis-decodes every property that follows.
    // These tests pin the deliberate behaviour: truncate + log (finish) or an
    // explicit error (finish_checked), never a wrap — for BOTH ceilings.
    // -----------------------------------------------------------------------

    /// A 70 KB String must NOT wrap the u16 prefix: the stream stays parseable and
    /// every property AFTER the oversize one still decodes to its true value.
    #[test]
    fn oversize_string_truncates_deliberately_instead_of_wrapping() {
        const OVERSIZE: usize = 70_000;
        let big = "A".repeat(OVERSIZE);
        let mut w = NetDataWriter::new();
        w.int(0, 4242).string(1, big).byte(2, 7).int(3, -99);

        // The explicit-error API names the offending property.
        assert_eq!(
            w.finish_checked().unwrap_err(),
            vec![NetDataOverflow { prop_id: 1, len: OVERSIZE, limit: NETDATA_MAX_STRING_LEN }],
            "finish_checked must reject the oversize propId, not silently emit it",
        );

        let bytes = w.finish();
        let p = parse_netdata(&bytes);
        assert!(
            p.ok,
            "the truncated stream must still parse cleanly (a wrapped u16 prefix leaves \
             ~65 KB of payload bytes to be misread as properties)",
        );
        assert_eq!(
            p.string(1).map(str::len),
            Some(NETDATA_MAX_STRING_LEN),
            "the oversize value is clamped to the u16 ceiling, not wrapped to {} B",
            OVERSIZE as u16,
        );
        // The load-bearing assertions: the properties AFTER the oversize one are
        // intact. With `len as u16` these decode from inside the payload ('A' = 65).
        assert_eq!(p.int(0), Some(4242), "propId 0 (before the oversize value)");
        assert_eq!(p.int(2), Some(7), "propId 2 AFTER the oversize value must survive");
        assert_eq!(p.int(3), Some(-99), "propId 3 AFTER the oversize value must survive");
    }

    /// Same guard for `ByteArray` — but at ITS ceiling, which is 255 B, not 64 KB.
    /// The truncation is byte-exact (a prefix of the original), so the reader gets
    /// a valid — if lossy — value.
    #[test]
    fn oversize_bytearray_truncates_deliberately_instead_of_wrapping() {
        const OVERSIZE: usize = 1_000; // over the u8 ceiling, far under the u16 one
        let big: Vec<u8> = (0..OVERSIZE as u32).map(|i| (i % 251) as u8).collect();
        let mut w = NetDataWriter::new();
        w.put(1, NetDataValue::ByteArray(big.clone())).byte(2, 0xAB);

        assert_eq!(
            w.finish_checked().unwrap_err(),
            vec![NetDataOverflow {
                prop_id: 1,
                len: OVERSIZE,
                limit: NETDATA_MAX_BYTEARRAY_LEN,
            }],
            "a ByteArray overflows at 255 B — the OLD u16 assumption would have let \
             this through and emitted an unparseable frame",
        );
        let p = parse_netdata(&w.finish());
        assert!(p.ok);
        match p.get(1) {
            Some(NetDataValue::ByteArray(b)) => {
                assert_eq!(b.len(), NETDATA_MAX_BYTEARRAY_LEN);
                assert_eq!(b[..], big[..NETDATA_MAX_BYTEARRAY_LEN], "a true prefix of the input");
            }
            other => panic!("expected a ByteArray at propId 1, got {other:?}"),
        }
        assert_eq!(p.int(2), Some(0xAB), "the property after the oversize one survives");
    }

    /// A `ByteArray` at and just under its own 255 B ceiling round-trips exactly,
    /// and the property after it stays intact.
    #[test]
    fn bytearray_at_its_u8_ceiling_roundtrips() {
        for len in [0usize, 1, 23, NETDATA_MAX_BYTEARRAY_LEN - 1, NETDATA_MAX_BYTEARRAY_LEN] {
            let blob: Vec<u8> = (0..len as u32).map(|i| (i % 251) as u8).collect();
            let mut w = NetDataWriter::new();
            w.put(1, NetDataValue::ByteArray(blob.clone())).byte(2, 0x5A);
            assert!(w.finish_checked().is_ok(), "len {len} must not be flagged");
            let bytes = w.finish();
            // One length byte, not two.
            assert_eq!(bytes.len(), 1 + 1 + 1 + 1 + len + 1, "len {len}: u8 prefix only");
            let p = parse_netdata(&bytes);
            assert!(p.ok, "len {len} must parse");
            assert_eq!(p.get(1), Some(&NetDataValue::ByteArray(blob)), "len {len} round-trips");
            assert_eq!(p.int(2), Some(0x5A), "len {len}: following property intact");
        }
    }

    /// Truncation of a `String` never splits a UTF-8 code point — the decoded
    /// value is still valid UTF-8 (no replacement chars from a half character).
    #[test]
    fn oversize_string_truncates_on_a_utf8_boundary() {
        // 3-byte code points: 65535 is NOT a multiple of 3, so a naive cut at the
        // ceiling would land mid-character.
        let big = "→".repeat(30_000); // 90 000 B
        let mut w = NetDataWriter::new();
        w.string(1, big);
        let p = parse_netdata(&w.finish());
        assert!(p.ok);
        let s = p.string(1).expect("string at propId 1");
        assert_eq!(s.len() % 3, 0, "cut on a code-point boundary (65535 % 3 == 0 is false)");
        assert!(s.len() <= NETDATA_MAX_STRING_LEN && s.len() > NETDATA_MAX_STRING_LEN - 3);
        assert!(s.chars().all(|c| c == '→'), "no partial code point / replacement char");
    }

    /// Non-breaking: values AT the ceiling and just under it encode exactly as
    /// before (no truncation, no error) — normal-size payloads are untouched.
    #[test]
    fn at_and_under_the_ceiling_is_unchanged() {
        for len in [0usize, 1, 4096, NETDATA_MAX_STRING_LEN - 1, NETDATA_MAX_STRING_LEN] {
            let s = "x".repeat(len);
            let mut w = NetDataWriter::new();
            w.string(1, s.clone()).byte(2, 9);
            assert!(w.finish_checked().is_ok(), "len {len} must not be flagged");
            let p = parse_netdata(&w.finish());
            assert!(p.ok, "len {len} must parse");
            assert_eq!(p.string(1), Some(s.as_str()), "len {len} round-trips exactly");
            assert_eq!(p.int(2), Some(9), "len {len}: following property intact");
        }
    }
}
