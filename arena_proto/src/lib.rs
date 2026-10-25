//! `arena_proto` — pure byte-layer for the Blades arena UDP protocol.
//!
//! Ports the protocol logic the capture platform already validated:
//!   - `scripts/arena-decrypt.py` — ChaCha20 stream crypto, ENet walk +
//!     plaintext reconstruction, fragment reassembly-before-decrypt, the
//!     first-opcode gate.
//!   - `emulator/internal/proto/{enet,opcodes}.go` — the ENet wire walker and
//!     the `GameMessageId` enum (codegen'd from `reference/il2cpp/arena-opcodes.json`).
//!
//! Everything here is **pure** (no tokio, no shared state, no I/O) so the
//! `server` crate, unit tests, and the offline replay/parity harness all share
//! the exact same bytes. The replay harness asserts this Rust pipeline
//! reproduces, byte-for-byte, the `plaintext`/`opcode` the Python worker wrote
//! into `arena_udp_frames`.

pub mod crypto;
pub mod enet;
pub mod netdata;
pub mod opcodes;
pub mod reassembly;

pub use crypto::{
    CryptoCtx, FixedNonce, NonceSource, UnresolvedNonce, chacha20_legacy, chacha20_legacy_xor,
    x25519_public, x25519_shared,
};
pub use enet::{
    STREAM_PLAINTEXT_LEADS, first_marker_in_plaintext, first_opcode_in_plaintext,
    reconstruct_plaintext, walk_fragments,
};
pub use netdata::{
    NETDATA_MAX_BYTEARRAY_LEN, NETDATA_MAX_STRING_LEN, NetDataOverflow, NetDataParse, NetDataType,
    NetDataValue, NetDataWriter, parse_netdata,
};
pub use opcodes::{GAME_MESSAGE_IDS, GameMessageId, is_game_message_id};
pub use reassembly::{
    Frame, KeyCandidate, REASSEMBLY_UUID_CONFIRM_MIN, group_decode_is_real, reassemble_session,
    resolve_fragment, select_endpoint_key, try_assembled, uuid_string_count,
};
