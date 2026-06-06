//! Live arena ENet host — the **real-client** path.
//!
//! Transport: **`rusty_enet`** (pure-Rust ENet port). Chosen over the C-backed
//! `tokio-enet` because that crate's socket layer is Linux-only
//! (`socket2::Type::cloexec`) and fails to build on macOS — blocking local dev.
//! `rusty_enet` is cross-platform, transport-agnostic (it drives our own UDP
//! socket), and **inspectable** — so if Blades' ENet header-flag quirk (`0x4000`
//! sentTime vs vanilla `0x8000`, per `arena-protocol-spec.md` §5) bites interop,
//! we can patch the parse. The retail client ships `libenet.so` → standard ENet,
//! so `rusty_enet` should interop.
//!
//! Crypto/handshake/FSM layer on top via `arena_proto` + `MatchRegistry`: once
//! the ENet connection establishes, the pubkey + nonce ride as **plaintext
//! channel-0 (`0x84`) messages** (`dump.cs` `Connection.{CreateKeyPair,
//! ComputeSecretAndActivateEncryption,GenerateNonce,Get/SetNonce}`), then SEND_*
//! user-data is ChaCha20 (counter 0).
//!
//! INTEGRATION PENDING (next step): bind a non-blocking UDP socket, build a
//! `rusty_enet::Host`, poll `service()`, and route Connect/Receive/Disconnect
//! through the registry (admit → decode → FSM → encode). This stub keeps the
//! live host wired into startup meanwhile; the raw-socket `UdpServer` + crypto/
//! FSM unit tests in `udp.rs` already prove the on-the-wire pipeline end-to-end.

use std::sync::Arc;

use log::info;

use crate::ServerGlobal;

pub async fn run_enet_host(globals: Arc<ServerGlobal>) -> anyhow::Result<()> {
    let port = globals.arena.config.udp_port;
    info!(
        "arena-enet: host pending rusty_enet integration (will bind udp/{port}, max {} matches)",
        globals.arena.registry.max_matches
    );
    Ok(())
}
