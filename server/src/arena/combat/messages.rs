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

use super::state::{ActiveSide, DamageSource, DamageType, FlowState, MatchState, NetObjectType, NetRole};

/// `NetTransportMessage.MAGIC_HEADER` — present on every message, both directions.
pub const MARKER_S2C: u8 = 0xBE;

/// Carrier MessageType for the "UserMessage" family (flow stateName, swipe,
/// ability, damage — disambiguated by body structure).
pub const MSGTYPE_USERMESSAGE: u8 = 0x36; // 54
/// Carrier MessageType for `CombatScreenInfo`.
pub const MSGTYPE_COMBAT_SCREEN: u8 = 0x37; // 55
/// Carrier MessageType for the match CLOCK (op58) — the FIRST s2c frame of the
/// round-start. Without it the client never starts its match timeline and sits at
/// "Connecting…". [RE'd byte-for-byte from s486.]
pub const MSGTYPE_CLOCK: u8 = 0x3a; // 58

/// The GameMessageId (NetData propId 3) carried by a flow-control stateName frame:
/// `MatchStateChangeRequest` = 79 (`0x4F`) server→client, `MatchStateChangeAck` =
/// 80 (`0x50`) client→server (the echo). This is NOT a "selector" — it is the real
/// GameMessageId (`dump.cs:588371-2`, `MatchStateChangeRequestMessage`/`AckMessage`
/// each carry one `string _stateTrigger`). The server drives the replicated
/// `Match.MatchState` purely by sending op79 with the trigger string; the client
/// Ack's with op80. Capture-proven byte-for-byte vs s506 #3522385/#3522389: an op79
/// "BackendMatchCreated" then a c2s op80 "BackendMatchCreated". [docs §7]
const GMID_MATCH_STATE_CHANGE_REQUEST: u8 = 79; // 0x4F, s2c
const GMID_MATCH_STATE_CHANGE_ACK: u8 = 80; // 0x50, c2s echo

/// Wrap a NetData `body` as a complete s2c `user_data`: `0xBE ‖ msg_type ‖ body`.
fn frame(msg_type: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + body.len());
    out.push(MARKER_S2C);
    out.push(msg_type);
    out.extend_from_slice(&body);
    out
}

/// Test helper: wrap a raw NetData `body` as a UserMessage (carrier `0x36`) frame —
/// for synthesizing inbound c2s handshake frames in engine tests.
#[cfg(test)]
pub(crate) fn frame_for_test(body: Vec<u8>) -> Vec<u8> {
    frame(MSGTYPE_USERMESSAGE, body)
}

/// A flow-control stateName message on the match flow-controller net object —
/// e.g. `BackendMatchCreated`, `StateTimeout`, `NextState`, `RoundEnd`. This is
/// how the server drives the match/round state machine **and the replicated
/// `Match.MatchState`**: it's a `MatchStateChangeRequest` (GameMessageId 79) on
/// the Control net object (`NetRole::None`), carrying the state trigger string the
/// client maps onto its `MatchState`/`PvpState` machine (e.g. the
/// `AwaitingClientBackendSynchronization`→`SynchronizingLoadout` advance). The
/// client echoes a `MatchStateChangeAck` (80). Server-authoritative.
///
/// `flow_controller_id` is the Control net object the server assigns for the
/// match (s293 used 436, s506 used 119). Returns `None` for the synthetic
/// [`FlowState`]s that have no wire string (`Connecting`/`Spawning`/`Finished`).
pub fn flow_state(flow_controller_id: i32, state: FlowState) -> Option<Vec<u8>> {
    Some(match_state_change_request(flow_controller_id, state.wire_name()?))
}

/// op79 `MatchStateChangeRequest` (carrier `0x36`) on the Control net object: the
/// server's authoritative request to advance the replicated `Match.MatchState` /
/// the client's `PvpState` machine, identified by a `_stateTrigger` STRING (NOT a
/// numeric enum on the wire). `dump.cs:590426` (`MatchStateChangeRequestMessage`,
/// `string _stateTrigger`). NetData `{0:Int controller · 1:Byte 57 Control · 2:Byte
/// 0 None · 3:Byte 79 · 4:String trigger}`. Byte-for-byte vs s506 #3522385
/// (trigger "BackendMatchCreated"). The MatchState `AwaitingClientBackendSynchronization`(9)
/// → `SynchronizingLoadout`(10) promotion the client mirrors is driven by this
/// message's trigger string (the numeric 9/10 are client-internal `MatchState.State`
/// values, never serialized — see docs/arena-journey-log.md §7).
pub fn match_state_change_request(controller_id: i32, trigger: &str) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, controller_id)
        .byte(1, NetObjectType::Control as u8)
        .byte(2, NetRole::None as u8)
        .byte(3, GMID_MATCH_STATE_CHANGE_REQUEST)
        .string(4, trigger);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// op80 `MatchStateChangeAck` (carrier `0x36`) — the CLIENT's echo of an op79 on
/// the Control object (`NetRole::Autonomous`, GameMessageId 80, same trigger
/// string). `dump.cs:590456`. The server does not normally SEND this (it's the
/// client→server ack); provided for completeness + the round-start differential.
/// Byte-for-byte vs s506 #3522389 (c2s ack of "BackendMatchCreated").
pub fn match_state_change_ack(controller_id: i32, trigger: &str) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, controller_id)
        .byte(1, NetObjectType::Control as u8)
        .byte(2, NetRole::Autonomous as u8)
        .byte(3, GMID_MATCH_STATE_CHANGE_ACK)
        .string(4, trigger);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// op61 `LoadoutClientBackendSynchronized` (carrier `0x36`) — `dump.cs:590190`
/// (`LoadoutClientBackendSynchronizedMessage : GameMessage`, single field
/// `bool HideHelmet`). NetData `{0:Int playerObj · 1:Byte 55 Player · 2:Byte role ·
/// 3:Byte 61 · 4:Bool HideHelmet}`, on the **Player** net object.
///
/// **Direction note (capture-proven):** in EVERY captured retail match (s127, 167,
/// 293, 385, 486, 503, 504, 506) this message is **client→server only** — the
/// client reports its own loadout-backend sync (with the helmet-cosmetic flag) at a
/// round transition; the server NEVER sends it. So this builder exists to (a) decode
/// the inbound c2s frame (see [`is_loadout_backend_synchronized`]/the engine's
/// non-combat gate) and (b) round-trip-prove the layout — it is NOT broadcast s2c at
/// round-start. Byte-for-byte vs s506 #3523229 (c2s, role 3, HideHelmet=true).
/// [docs/arena-journey-log.md §7]
pub fn loadout_client_backend_synchronized(
    player_net_object_id: i32,
    role: NetRole,
    hide_helmet: bool,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, player_net_object_id)
        .byte(1, NetObjectType::Player as u8)
        .byte(2, role as u8)
        .byte(3, GameMessageId::LoadoutClientBackendSynchronized as u8)
        .bool(4, hide_helmet);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// The GameMessageId carried at NetData propId 3 of a carrier-`0x36` user-message
/// (the real discriminator — carrier `0x36` is shared across the whole UserMessage
/// family). `None` if the frame isn't carrier `0x36` or has no integral propId 3.
/// Used by the engine to tell a round-start HANDSHAKE/state frame (op20/22/36/56/61/
/// 79/80 …) apart from an actual combat swing/ability before resolving damage.
pub fn user_message_gmid(user_data: &[u8]) -> Option<u8> {
    if user_data.get(1) != Some(&MSGTYPE_USERMESSAGE) {
        return None;
    }
    arena_proto::parse_netdata(user_data.get(2..)?)
        .int(3)
        .and_then(|v| u8::try_from(v).ok())
}

/// True iff a carrier-`0x36` c2s frame is the client's `LoadoutClientBackendSynchronized`
/// (op61) — a round-transition handshake signal, NOT a combat input.
pub fn is_loadout_backend_synchronized(user_data: &[u8]) -> bool {
    user_message_gmid(user_data) == Some(GameMessageId::LoadoutClientBackendSynchronized as u8)
}

/// The retail **ENet channel** a given decrypted `user_data` (`marker ‖ MessageType
/// ‖ body`) must be sent on, matching session 506 byte-for-byte. Blades' client
/// binds different NetTransport message classes to different ENet channels; if the
/// server sends a message on the wrong channel the client's per-channel receive
/// path never dispatches it (`NetObjectModule.OnUserMessage` doesn't fire) and it
/// hangs at "Connecting…".
///
/// Channel map (extracted from s506 round-start, both directions — ENet command
/// header byte +1 = channelID; client CONNECT negotiates `channelCount=7`, so
/// ch0–6 are all valid):
///   - **ch4** — the big `OpponentLoadout` profile (carrier 0x36, GMID 35,
///     ~20–30 KB, fragmented) and `MatchEndMatchMsg` (GMID 49). [s506 #3521912-ish
///     ch4 GMID 35 ×2, GMID 49 ×1]
///   - **ch1** — the per-player stat words: `PlayerStatsUpdate` (GMID 65) and
///     `PlayerDestroyedStatUpdate` (GMID 75). [s506 ch1 GMID 65 ×8, GMID 75 ×67]
///   - **ch6** — combat input (`PlayerCombatInputActivate`/`Position`, GMID 46/47);
///     c2s in retail, mapped for symmetry though the server doesn't emit them.
///   - **ch0** — EVERYTHING else: spawns (0x32), op55 (0x35/0x37), op58 clock
///     (0x3a), 0x33/0x39, and carrier-0x36 for all other GMIDs (PlayerWelcome 21,
///     SpawnAvatar 22, PlayerLoadoutReady 36, state changes 39/79/80, ReceiveDamage
///     50, …). [s506 ch0, the overwhelming majority both directions]
///
/// This replaces the old "route by ciphertext length (>1000 ⇒ ch4 else ch0)"
/// heuristic in `enet_host.rs`, which never used ch1 — so `PlayerStatsUpdate`
/// (small, <1000 B) wrongly went on ch0.
pub fn retail_channel(user_data: &[u8]) -> u8 {
    // Carrier-0x36 family: discriminate by the GameMessageId at propId 3.
    if user_data.get(1) == Some(&MSGTYPE_USERMESSAGE) {
        match user_message_gmid(user_data) {
            Some(35) | Some(49) => return 4, // OpponentLoadout profile / MatchEnd
            Some(65) | Some(75) => return 1, // PlayerStatsUpdate / PlayerDestroyedStatUpdate
            Some(46) | Some(47) => return 6, // combat input (c2s; symmetry only)
            _ => return 0,
        }
    }
    // All other carriers (spawns 0x32, op55 0x35/0x37, clock 0x3a, 0x33/0x39, …)
    // ride channel 0 in retail.
    0
}

/// Carrier-`0x36` GameMessageIds that are **round-start / round-transition handshake
/// or flow-control signals, NOT combat inputs** — the server must NOT resolve them
/// as a weapon swing or it injects phantom damage during setup / between rounds.
///
/// Capture-proven from s506's c2s carrier-`0x36` traffic (a live PvP match): the real
/// combat inputs are `RequestExecuteAbility`(37), `PlayerCombatInputActivate`(46) and
/// `PlayerCombatInputPosition`(47); everything else on this carrier is handshake —
/// `PlayerInfo`(20), `PlayerSpawnAvatar`(22), `PlayerLoadoutReady`(36),
/// `EquipAbilitiesAndConsumables`(56), `SkipCurrentState`(57),
/// `LoadoutClientBackendSynchronized`(61), `MatchStateChangeRequest`(79),
/// `MatchStateChangeAck`(80), emotes (72/73). [docs/arena-journey-log.md §7]
pub fn is_noncombat_user_message(user_data: &[u8]) -> bool {
    matches!(
        user_message_gmid(user_data),
        Some(
            20 | 22 | 36 | 56 | 57 | 61 | 72 | 73 | 76 | 79 | 80
        )
    )
}

/// op58 (carrier `0x3a`) — the match CLOCK: two `Long` (.NET `DateTime.Ticks`,
/// 100 ns since year 1) at propIds 0/1. The retail server sends this **first** at
/// round-start; the client needs it to start the match timeline — without it the
/// client sits at "Connecting…" (the 2026-06-17 paired-match stall). s486 carried
/// the two values ~0.84 s apart (server clock vs match-start ref); both ≈ "now"
/// works. [RE'd byte-for-byte from s486 / docs §6.2.]
pub fn clock(tick_clock: i64, tick_match_start: i64) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.long(0, tick_clock).long(1, tick_match_start);
    frame(MSGTYPE_CLOCK, w.finish())
}

/// Carrier for a type-54 Match net-object **property update** (op `0x35`). The
/// client's `Match.OnObjectPropertiesChanged` applies the new NetData to the
/// already-spawned Match object — this is how the replicated `MatchState` advances.
pub const MSGTYPE_NETOBJ_UPDATE: u8 = 0x35; // 53/55 family — net-object property change

/// `MaxMatchRounds` — the Match net-object's propId8 (s506: 3, a best-of-3 arena).
pub const MATCH_MAX_ROUNDS: u8 = 3;
/// The Match net-object's propId3 — a constant `Int 21` in every s506 Match frame
/// (purpose unconfirmed; near-constant, not the binding gate). Kept verbatim.
const MATCH_PROP3: i32 = 21;

/// NetData for the single type-54 **Match** net object (s506 obj 123) — the object
/// whose **propId5 = `MatchState`** the client reads to bind its players and advance
/// the match. Capture-proven field layout (byte-diffed against s506 obj 123 across
/// its spawn + every op55 update): `{0:Int id · 1:Byte 54 (Match) · 2:Byte role ·
/// 3:Int 21 · 4:Byte playerCount · 5:Byte MatchState · 6:Float stateTimeoutSeconds ·
/// 7:Byte currentRound · 8:Byte maxRounds · 9:String gameSessionId}`.
///
/// This REPLACES the fork's old per-fighter "ability" type-54 object, which used the
/// same wire shape but hard-coded propId5 = 5 (`BackendMatchCreation`) and a per-
/// fighter ability UUID at propId9. That made the client jump `MatchState` Idle→5,
/// skip `WaitingForPlayers`(3)/`InitialPlayerSetup`(4), and never bind its players.
#[allow(clippy::too_many_arguments)]
fn match_netdata(
    net_object_id: i32,
    role: NetRole,
    player_count: u8,
    state: MatchState,
    state_timeout_secs: f32,
    current_round: u8,
    game_session_id: &str,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, net_object_id)
        .byte(1, NetObjectType::Match as u8)
        .byte(2, role as u8)
        .int(3, MATCH_PROP3)
        .byte(4, player_count)
        .byte(5, state as u8)
        .float(6, state_timeout_secs)
        .byte(7, current_round)
        .byte(8, MATCH_MAX_ROUNDS)
        .string(9, game_session_id);
    w.finish()
}

/// op50 SPAWN (carrier `0x32`) of the type-54 Match net object. Spawned at round
/// start with `MatchState::WaitingForPlayers`(3) so the client constructs its `Match`
/// object and begins binding the local/opponent `PvpPlayer` (s506: spawn role 2
/// Simulated, propId5 = 3, propId6 = 20s). Subsequent state changes use
/// [`update_match`].
#[allow(clippy::too_many_arguments)]
pub fn spawn_match(
    net_object_id: i32,
    player_count: u8,
    state: MatchState,
    state_timeout_secs: f32,
    current_round: u8,
    game_session_id: &str,
) -> Vec<u8> {
    frame(
        MSGTYPE_SPAWN,
        match_netdata(
            net_object_id,
            NetRole::Simulated,
            player_count,
            state,
            state_timeout_secs,
            current_round,
            game_session_id,
        ),
    )
}

/// op55 (carrier `0x35`) Match net-object **property update** — advances the
/// replicated `MatchState` (propId5) on the already-spawned Match object. s506
/// drives 3→4→5→6→7→11 with this; role flips to 1 (Authority) on updates.
#[allow(clippy::too_many_arguments)]
pub fn update_match(
    net_object_id: i32,
    player_count: u8,
    state: MatchState,
    state_timeout_secs: f32,
    current_round: u8,
    game_session_id: &str,
) -> Vec<u8> {
    frame(
        MSGTYPE_NETOBJ_UPDATE,
        match_netdata(
            net_object_id,
            NetRole::Authority,
            player_count,
            state,
            state_timeout_secs,
            current_round,
            game_session_id,
        ),
    )
}

/// op54-small (carrier `0x36`) — per-avatar stat/HP word:
/// `{0:Int avatar_id · 1:Byte 56 (Avatar) · 2:Byte 1 · 3:Byte 65 · 4:ULong (full
/// Health|Stamina|Magicka in hi32 | seq=1 lo32) · 5:ULong 1}`. Full at round-start.
/// [RE'd byte-exact from s486.]
pub fn stat_update(avatar_net_object_id: i32) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, avatar_net_object_id)
        .byte(1, 56)
        .byte(2, 1)
        .byte(3, 65)
        .ulong(4, 0x3FFF_FFFF_0000_0001)
        .ulong(5, 1);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// op21 `PlayerWelcome` (carrier `0x36`) on the viewer's OWN Player net object —
/// the FIRST carrier-`0x36` user-message of the round-start. The retail server
/// sends this right after the spawns; the client's `PvpPlayer` needs it to enter
/// the user-message / loadout-upload phase. Without it the client receives the
/// op50 spawns (and ACKs them) but `NetObjectModule.OnUserMessage` never fires →
/// it never uploads its loadout (op54) and hangs at "Connecting…".
///
/// NetData `{0:Int playerObj · 1:Byte 55 Player · 2:Byte 1 Authority · 3:Byte 21
/// (PlayerWelcome gmid) · 4:Byte param}`. Byte-for-byte vs s506 #3522332 (obj 120,
/// p4=21) / #3521912 (obj 116, p4=20). **p4 semantics UNCONFIRMED** — observed 20/21
/// across s506+s477, NOT correlated with obj id or level (so not the arena rank);
/// a small near-constant per-player arena-state byte. Defaulted to the most common
/// observed value (20); refine if a capture pins its meaning. [diffed 2026-06-19:
/// op21 is the carrier-0x36 message the fork was MISSING vs retail s506.]
pub const GMID_PLAYER_WELCOME: u8 = 21;
pub fn player_welcome(player_net_object_id: i32, param: u8) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, player_net_object_id)
        .byte(1, NetObjectType::Player as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GMID_PLAYER_WELCOME)
        .byte(4, param);
    frame(MSGTYPE_USERMESSAGE, w.finish())
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

/// op54 (carrier `0x36`) — the per-player **PROFILE**: the full character + equipped
/// gear as JSON, so the client can construct the (opponent's) avatar — appearance,
/// gear, abilities, PvP stats. `equipped_items_json` = `{"equippedItems":{…}}`;
/// `character_json` = the character (`id`+`name`+`tagId`+`equippedAbilities`+
/// `abilities`+customization+PvP stats). Tens of KB → ENet fragments it (rusty_enet
/// auto-fragments a reliable packet). Decoded from the reassembled s486 op54
/// (docs/arena-protocol-spec.md §6.2). NetData: p0=player obj id, p1=55 Player,
/// p2=1 (Authority), p3=35 (the profile GameMessageId), p4/p5=the JSON, p6=Bool.
///
/// **p6 = `false`** — capture-proven from the reassembled s506 op54 PROFILE (the
/// last byte after the closing `}` of the character JSON is `0x00`). The original
/// implementation guessed `true`; retail sends `false`. [diffed 2026-06-19 against
/// s506 player B "Blank" profile (16 fragments, 20776 B).]
pub fn player_profile(player_net_object_id: i32, equipped_items_json: &str, character_json: &str) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, player_net_object_id)
        .byte(1, NetObjectType::Player as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, 35) // the profile message's GameMessageId (propId 3)
        .string(4, equipped_items_json)
        .string(5, character_json)
        .bool(6, false);
    frame(MSGTYPE_USERMESSAGE, w.finish())
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

/// `PlayerDeadStateChange` (29) — the addressed avatar died (the killing blow).
///
/// **Capture-proven layout (s506 #3523661, the final-round death):** op29 rides the
/// UserMessage carrier `0x36` (NOT its own carrier as the old placeholder guessed) —
/// it is one of the avatar-state-change family on the Avatar net-object, GMID at
/// propId 3. NetData `{0:Int deadAvatarObj · 1:Byte 56 Avatar · 2:Byte 1 Authority ·
/// 3:Byte 29 · 4:ULong deadActorPackedStats · 5:ULong otherActorPackedStats ·
/// 6:Byte cause}` — the same NetObjectInfo + two packed-stats ULong shape as
/// `ReceiveDamage`/the 41-45/52 state changes, minus the damage components.
///
/// `dead_packed_stats`/`other_packed_stats` are the two actors' current packed pools
/// (`Fighter::packed_stats`); `cause` is a small byte (s506 = 3, the killing blow's
/// DamageSource — WeaponManeuver — observed; not the binding field). Byte-for-byte vs
/// s506 #3523661 (obj 124, p6=3). [decoded from prod arena_udp_frames s506 2026-06-19;
/// supersedes the prior UNVERIFIED bare-NetObjectInfo guess.]
pub fn player_dead(
    dead_avatar_net_object_id: i32,
    dead_packed_stats: u64,
    other_packed_stats: u64,
    cause: u8,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, dead_avatar_net_object_id)
        .byte(1, NetObjectType::Avatar as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::PlayerDeadStateChange as u8)
        .ulong(4, dead_packed_stats)
        .ulong(5, other_packed_stats)
        .byte(6, cause);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// `MatchPostRoundInfoMsg` (48) — the round/match RESULT, sent at the PostRound
/// transition on the **Match** net-object. **This is the real retail "who won"
/// message** (s506 sends op48, never op49): the client reads it to show the
/// result/victory screen.
///
/// **Capture-proven layout (s506 #3523671, the final round of a best-of-3):** carrier
/// `0x36`, on Match obj 123 (type 54, Authority). NetData
/// `{0:Int matchObj · 1:Byte 54 · 2:Byte 1 · 3:Byte 48 · 4:Int 3 · 5:String winnerCharUUID
/// · 6:String loserCharUUID · 7:String winnerCharUUID · 8:String loserCharUUID ·
/// 9:String "" · 10:String "" · 11:Byte 1 · 12:String winnerCharUUID · 13:String
/// loserCharUUID · 14:Bool false · 15:Bool true · 16:String winnerCharUUID · 17:Bool
/// false · 18:String matchId}`. The winner UUID repeats at p5/p7/p12/p16 and the loser
/// at p6/p8/p13 (the client cross-checks them); p4 = a small Int (s506 = 3, the match's
/// maxRounds / a result code — near-constant), p18 = the matchId. Byte-for-byte vs s506
/// #3523671 (winner 1131a037…, loser 38c987fd…). [decoded from prod arena_udp_frames
/// s506 2026-06-19.]
#[allow(clippy::too_many_arguments)]
pub fn match_post_round_info(
    match_net_object_id: i32,
    winner_char_uuid: &str,
    loser_char_uuid: &str,
    match_id: &str,
    result_code: i32,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, match_net_object_id)
        .byte(1, NetObjectType::Match as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::MatchPostRoundInfoMsg as u8)
        .int(4, result_code)
        .string(5, winner_char_uuid)
        .string(6, loser_char_uuid)
        .string(7, winner_char_uuid)
        .string(8, loser_char_uuid)
        .string(9, "")
        .string(10, "")
        .byte(11, 1)
        .string(12, winner_char_uuid)
        .string(13, loser_char_uuid)
        .bool(14, false)
        .bool(15, true)
        .string(16, winner_char_uuid)
        .bool(17, false)
        .string(18, match_id);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// `MatchEndMatchMsg` (49) — **NOT sent by retail at match-end.** Decoded s506
/// end-to-end: the match RESULT is delivered via [`match_post_round_info`] (op48) at
/// PostRound, and a large fragmented `ResultsJSON` rides separate carriers (0xc2/0xc6)
/// during BackendMatchEnd(17). op49 never appears as a discrete command in any
/// captured match (s127/167/293/385/486/503/504/506). Retained as a minimal builder
/// (NetObjectInfo + winner id) only for the `GameMessageId` round-trip / tests — it is
/// **not** emitted by the engine. [decoded from prod arena_udp_frames s506 2026-06-19.]
pub fn match_end(winner_net_object_id: i32) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.net_object_info(winner_net_object_id, NetObjectType::Match as u8, NetRole::Authority as u8)
        .int(3, winner_net_object_id);
    frame(GameMessageId::MatchEndMatchMsg as u8, w.finish())
}

/// `PlayerEmoteStateChange` (73) — the s2c relay of a client's `PlayEmote` (72).
/// `dump.cs:590906` (`PlayerEmoteStateChangeMessage : PlayerStateChangeMessage`,
/// fields `ActorStateType.StateId stateId` + `string emoteId`). It is one of the
/// avatar-state-change family on the EMOTING actor's net-object (the same
/// NetObjectInfo + StateId shape as the other `Player*StateChange`), carrying the
/// `emoteId` string the client maps to the emote animation. We relay it to the
/// OPPONENT so the emote displays on the other player's screen.
///
/// NetData `{0:Int actorObj · 1:Byte 56 Avatar · 2:Byte 1 Authority · 3:Byte 73
/// (PlayerEmoteStateChange gmid) · 4:Byte stateId (Emote=28) · 5:String emoteId}`.
/// The exact wire prop count of the `PlayerStateChange` base (the optional
/// `_stateHistory`/`_timeInPreviousState`) is build-specific and not pinned from a
/// two-sided capture; this minimal NetObjectInfo + StateId + emoteId shape is what
/// the client needs to render the opponent's emote. [structure from dump.cs; the
/// raw c2s PlayEmote frame is not byte-decodable from the retained ENet-framed
/// captures — see the resolve.rs note.]
pub fn player_emote_state_change(emoting_avatar_net_object_id: i32, emote_id: &str) -> Vec<u8> {
    use super::state::ActorStateType;
    let mut w = NetDataWriter::new();
    w.int(0, emoting_avatar_net_object_id)
        .byte(1, NetObjectType::Avatar as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::PlayerEmoteStateChange as u8)
        .byte(4, ActorStateType::Emote as u8)
        .string(5, emote_id);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// Read the `emoteId` string a client's `PlayEmote` (72) carries. `PlayEmoteMessage`
/// (`dump.cs:588944`) has a single `string _emoteId`; on the wire that is the first
/// string property of the carrier-0x36 body. We don't know the exact propId the
/// client serializes it at across the NetObjectInfo header, so scan for the FIRST
/// string-typed property after propId 3 (the GameMessageId) and return it. `None`
/// if the frame isn't a `PlayEmote` or carries no string. (Best-effort decode of an
/// un-capture-pinned c2s frame; the relay degrades to an empty emoteId if absent.)
pub fn play_emote_id(user_data: &[u8]) -> Option<String> {
    if user_message_gmid(user_data) != Some(GameMessageId::PlayEmote as u8) {
        return None;
    }
    let nd = arena_proto::parse_netdata(user_data.get(2..)?);
    // The emoteId is a string property; take the first string prop id > 3.
    let mut keys: Vec<&u8> = nd.props.keys().filter(|k| **k > 3).collect();
    keys.sort();
    for k in keys {
        if let Some(s) = nd.string(*k) {
            return Some(s.to_string());
        }
    }
    Some(String::new()) // a PlayEmote with no decodable string → empty (still relay)
}

/// True iff a carrier-0x36 c2s frame is the client's `PlayEmote` (72).
pub fn is_play_emote(user_data: &[u8]) -> bool {
    user_message_gmid(user_data) == Some(GameMessageId::PlayEmote as u8)
}

/// True iff a carrier-0x36 c2s frame is a `PlayerBlockingStateChange` (41) — the
/// client raising/lowering its guard. `dump.cs:590637`
/// (`PlayerBlockingStateChangeMessage : PlayerStateChangeMessage`). The server reads
/// it to put the fighter into / out of the Blocking actor-state (so incoming hits
/// are reduced — see `damage::block_outcome`); it is NOT a swing.
pub fn is_player_blocking_state_change(user_data: &[u8]) -> bool {
    user_message_gmid(user_data) == Some(GameMessageId::PlayerBlockingStateChange as u8)
}

/// Read the `ActiveSide` (guard side) a `PlayerBlockingStateChange` (41) carries, if
/// present — the `PlayerStateChange` family puts a small side/param byte after the
/// GameMessageId. Returns the first byte-typed property > 3 (the block side). `None`
/// when absent → caller defaults to a generic (Middle) guard.
pub fn blocking_active_side(user_data: &[u8]) -> Option<ActiveSide> {
    if user_message_gmid(user_data) != Some(GameMessageId::PlayerBlockingStateChange as u8) {
        return None;
    }
    let nd = arena_proto::parse_netdata(user_data.get(2..)?);
    let mut keys: Vec<&u8> = nd.props.keys().filter(|k| **k > 3).collect();
    keys.sort();
    for k in keys {
        if let Some(v) = nd.int(*k) {
            return Some(match v {
                1 => ActiveSide::Middle,
                2 => ActiveSide::Left,
                3 => ActiveSide::Right,
                _ => ActiveSide::None,
            });
        }
    }
    None
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

    /// Byte-for-byte vs s486 round-start op58 (after `BE 3A`): two Longs
    /// (.NET DateTime.Ticks) at propIds 0/1 — `01 03 33` then the two LE i64s.
    #[test]
    fn clock_matches_s486_capture() {
        let got = clock(0x08DE_CB13_D7F6_FE1C, 0x08DE_CB13_D807_9C22);
        let want = [
            0xBE, 0x3A, // marker + clock carrier (58)
            0x01, 0x03, 0x33, // maxPropId=1, bitmap {0,1}, type nibbles [Long,Long]
            0x1C, 0xFE, 0xF6, 0xD7, 0x13, 0xCB, 0xDE, 0x08, // prop0 Long (server clock)
            0x22, 0x9C, 0x07, 0xD8, 0x13, 0xCB, 0xDE, 0x08, // prop1 Long (match-start ref)
        ];
        assert_eq!(got, want);
    }

    /// Byte-for-byte vs s506 #3522332 — the type-54 **Match** net-object SPAWN (after
    /// `BE 32`): obj 123, role 2 (Simulated), p3 Int 21, p4 Byte 1 (PlayerCount),
    /// **p5 Byte 3 (MatchState::WaitingForPlayers)**, p6 Float 20.0 (timeout), p7
    /// Byte 0 (round), p8 Byte 3 (maxRounds), p9 String gameSessionId. This is the
    /// object whose propId5 the client reads to bind its players — the gate the old
    /// per-fighter "ability" spawn broke by hard-coding p5 = 5.
    #[test]
    fn spawn_match_matches_s506() {
        let got = spawn_match(
            123,
            1,
            MatchState::WaitingForPlayers,
            20.0,
            0,
            "5b764e61-8851-4703-8fea-3d8e589ed24f",
        );
        let mut want = vec![
            0xBE, 0x32, // marker + SPAWN carrier (op50)
            0x09, 0xFF, 0x03, // maxPropId 9, bitmap {0..9}
            0x70, 0x07, 0x77, 0x75, 0xA7, // type nibbles [Int,Byte,Byte,Int,Byte,Byte,Float,Byte,Byte,String]
            0x7B, 0x00, 0x00, 0x00, // p0 Int = 123 (Match obj)
            0x36, // p1 Byte = 54 (Match)
            0x02, // p2 Byte = 2 (Simulated)
            0x15, 0x00, 0x00, 0x00, // p3 Int = 21
            0x01, // p4 Byte = 1 (PlayerCount)
            0x03, // p5 Byte = 3 (MatchState::WaitingForPlayers)
            0x00, 0x00, 0xA0, 0x41, // p6 Float = 20.0 (timeout)
            0x00, // p7 Byte = 0 (round)
            0x03, // p8 Byte = 3 (maxRounds)
            0x24, 0x00, // p9 String len = 36
        ];
        want.extend_from_slice(b"5b764e61-8851-4703-8fea-3d8e589ed24f");
        assert_eq!(got, want, "Match spawn must byte-match s506 obj 123 (p5=WaitingForPlayers)");
    }

    /// Byte-for-byte vs s506 #3522339 — the Match net-object **property UPDATE** (op55,
    /// after `BE 35`) that advances `MatchState` to InitialPlayerSetup(4): obj 123,
    /// role 1 (Authority), p5 Byte 4, p6 Float 30.0 (timeout). Same NetData shape as
    /// the spawn; only the carrier (0x35), role, p5 and p6 differ.
    #[test]
    fn update_match_matches_s506() {
        let got = update_match(
            123,
            2,
            MatchState::InitialPlayerSetup,
            30.0,
            0,
            "5b764e61-8851-4703-8fea-3d8e589ed24f",
        );
        let mut want = vec![
            0xBE, 0x35, // marker + net-object UPDATE carrier (op55)
            0x09, 0xFF, 0x03, // maxPropId 9, bitmap {0..9}
            0x70, 0x07, 0x77, 0x75, 0xA7, // type nibbles
            0x7B, 0x00, 0x00, 0x00, // p0 Int = 123
            0x36, // p1 Byte = 54 (Match)
            0x01, // p2 Byte = 1 (Authority — updates flip to Authority)
            0x15, 0x00, 0x00, 0x00, // p3 Int = 21
            0x02, // p4 Byte = 2 (PlayerCount)
            0x04, // p5 Byte = 4 (MatchState::InitialPlayerSetup)
            0x00, 0x00, 0xF0, 0x41, // p6 Float = 30.0 (timeout)
            0x00, // p7 Byte = 0
            0x03, // p8 Byte = 3
            0x24, 0x00, // p9 String len = 36
        ];
        want.extend_from_slice(b"5b764e61-8851-4703-8fea-3d8e589ed24f");
        assert_eq!(got, want, "Match update must byte-match s506 obj 123 (p5=InitialPlayerSetup)");
    }

    /// Byte-for-byte vs s506 #3522332 (s2c op21 PlayerWelcome, player B): the
    /// viewer's own Player obj 120, role Authority, gmid 21, p4=21.
    #[test]
    fn player_welcome_matches_s506() {
        let got = player_welcome(120, 21);
        let want = [
            0xBE, 0x36, // marker + UserMessage carrier
            0x04, 0x1F, // maxPropId=4, bitmap {0,1,2,3,4}
            0x70, 0x77, 0x07, // type nibbles [Int,Byte,Byte,Byte,Byte]
            0x78, 0x00, 0x00, 0x00, // p0 Int = 120 (player obj)
            0x37, // p1 Byte = 55 (Player)
            0x01, // p2 Byte = 1 (Authority)
            0x15, // p3 Byte = 21 (PlayerWelcome gmid)
            0x15, // p4 Byte = 21 (param)
        ];
        assert_eq!(got, want);
        // Player A's variant (obj 116, p4=20) — same shape, different values.
        let got_a = player_welcome(116, 20);
        assert_eq!(&got_a[7..11], &[0x74, 0x00, 0x00, 0x00], "p0 = 116");
        assert_eq!(got_a[14], 0x14, "p4 = 20");
    }

    /// Byte-for-byte vs s486 round-start op54-small (after `BE 36`).
    #[test]
    fn stat_update_matches_s486() {
        let got = stat_update(88);
        let want = [
            0xBE, 0x36, // marker + UserMessage carrier
            0x05, 0x3F, // maxPropId 5, bitmap {0..5}
            0x70, 0x77, 0x22, // types [Int,Byte,Byte,Byte,ULong,ULong]
            0x58, 0x00, 0x00, 0x00, // p0 Int = 88
            0x38, 0x01, 0x41, // p1 Byte 56, p2 Byte 1, p3 Byte 65
            0x01, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x3F, // p4 ULong full stats
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // p5 ULong 1
        ];
        assert_eq!(got, want);
    }

    /// The retail ENet channel map, locked to s506. A carrier-0x36 user-message
    /// with a small NetData body carrying propId3 = the GameMessageId; assert each
    /// GMID routes to the channel s506 used.
    #[test]
    fn retail_channel_matches_s506_map() {
        // Helper: build a carrier-0x36 frame whose propId3 (Byte) = `gmid`.
        let user_msg = |gmid: u8| -> Vec<u8> {
            let mut w = NetDataWriter::new();
            w.int(0, 1).byte(1, 55).byte(2, 1).byte(3, gmid);
            frame(MSGTYPE_USERMESSAGE, w.finish())
        };

        // ch4: the big OpponentLoadout profile (GMID 35) + MatchEnd (GMID 49).
        assert_eq!(retail_channel(&user_msg(35)), 4, "OpponentLoadout → ch4");
        assert_eq!(retail_channel(&user_msg(49)), 4, "MatchEndMatchMsg → ch4");
        // The real profile builder (large, fragmented) must also land on ch4.
        assert_eq!(retail_channel(&player_profile(7, "{}", "{}")), 4);

        // ch1: the per-player stat words.
        assert_eq!(retail_channel(&user_msg(65)), 1, "PlayerStatsUpdate → ch1");
        assert_eq!(retail_channel(&stat_update(88)), 1, "stat_update (GMID 65) → ch1");
        assert_eq!(retail_channel(&user_msg(75)), 1, "PlayerDestroyedStatUpdate → ch1");

        // ch6: combat input (c2s in retail; mapped for symmetry).
        assert_eq!(retail_channel(&user_msg(46)), 6);
        assert_eq!(retail_channel(&user_msg(47)), 6);

        // ch0: every other carrier-0x36 GMID + every non-0x36 carrier.
        assert_eq!(retail_channel(&player_welcome(120, 21)), 0, "PlayerWelcome (GMID 21) → ch0");
        assert_eq!(retail_channel(&user_msg(36)), 0, "PlayerLoadoutReady → ch0");
        assert_eq!(retail_channel(&user_msg(50)), 0, "ReceiveDamage → ch0");
        assert_eq!(retail_channel(&user_msg(79)), 0, "MatchStateChangeRequest → ch0");
        assert_eq!(retail_channel(&clock(0, 0)), 0, "op58 clock (carrier 0x3a) → ch0");
        assert_eq!(
            retail_channel(&combat_screen_info(437, NetObjectType::Player, NetRole::Simulated)),
            0,
            "op55 CombatScreenInfo (carrier 0x37) → ch0"
        );
        assert_eq!(retail_channel(&spawn_avatar(116, NetRole::Simulated, "x")), 0, "spawn (carrier 0x32) → ch0");
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

    /// op54 profile carries the two JSON blobs at p4/p5 + the structural fields.
    #[test]
    fn player_profile_structure() {
        let eq = r#"{"equippedItems":{}}"#;
        let ch = r#"{"id":"x","name":"Taheen"}"#;
        let got = player_profile(197, eq, ch);
        assert_eq!(&got[0..2], &[0xBE, 0x36], "marker + UserMessage carrier");
        let nd = arena_proto::parse_netdata(&got[2..]);
        assert_eq!(nd.int(0), Some(197), "p0 player obj id");
        assert_eq!(nd.int(1), Some(55), "p1 Player");
        assert_eq!(nd.int(3), Some(35), "p3 profile gameMessageId");
        assert_eq!(nd.string(4), Some(eq), "p4 equippedItems json");
        assert_eq!(nd.string(5), Some(ch), "p5 character json");
        // p6 == false (capture-proven vs s506; the reassembled op54 ends `}` then 0x00).
        assert_eq!(
            nd.props.get(&6),
            Some(&arena_proto::NetDataValue::Bool(false)),
            "p6 Bool must be false (retail s506), not true"
        );
        assert_eq!(*got.last().unwrap(), 0x00, "final wire byte is the p6=false bool");
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

    /// Byte-for-byte vs s506 #3522385 (s2c MatchStateChangeRequest, op79): the flow
    /// controller (obj 119, type 57 Control, role 0 None) with trigger
    /// "BackendMatchCreated". This is the SAME bytes `flow_state` emits — op79 IS the
    /// MatchState advance on the wire (no numeric enum).
    #[test]
    fn match_state_change_request_matches_s506() {
        let got = match_state_change_request(119, "BackendMatchCreated");
        let mut want = vec![
            0xBE, 0x36, // marker + UserMessage carrier
            0x04, 0x1F, // maxPropId=4, bitmap {0,1,2,3,4}
            0x70, 0x77, 0x0A, // type nibbles [Int,Byte,Byte,Byte,String]
            0x77, 0x00, 0x00, 0x00, // p0 Int = 119 (flow controller)
            0x39, // p1 Byte = 57 (Control)
            0x00, // p2 Byte = 0 (NetRole::None)
            0x4F, // p3 Byte = 79 (MatchStateChangeRequest)
            0x13, 0x00, // p4 String len = 19
        ];
        want.extend_from_slice(b"BackendMatchCreated");
        assert_eq!(got, want);
        // flow_state delegates to it → identical bytes for the same trigger.
        assert_eq!(flow_state(119, FlowState::BackendMatchCreated).unwrap(), got);
    }

    /// Byte-for-byte vs s506 #3522389 (c2s MatchStateChangeAck, op80): same controller,
    /// role 3 (Autonomous), gmid 80, same trigger string.
    #[test]
    fn match_state_change_ack_matches_s506() {
        let got = match_state_change_ack(119, "BackendMatchCreated");
        let mut want = vec![
            0xBE, 0x36, 0x04, 0x1F, 0x70, 0x77, 0x0A, //
            0x77, 0x00, 0x00, 0x00, // p0 = 119
            0x39, // p1 = 57 (Control)
            0x03, // p2 = 3 (Autonomous — client echo)
            0x50, // p3 = 80 (MatchStateChangeAck)
            0x13, 0x00, // p4 len 19
        ];
        want.extend_from_slice(b"BackendMatchCreated");
        assert_eq!(got, want);
    }

    /// Byte-for-byte vs s506 #3523229 (c2s op61 LoadoutClientBackendSynchronized):
    /// Player obj 120, role 3 (Autonomous), gmid 61, HideHelmet=true.
    #[test]
    fn loadout_backend_synchronized_matches_s506() {
        let got = loadout_client_backend_synchronized(120, NetRole::Autonomous, true);
        let want = [
            0xBE, 0x36, // marker + UserMessage carrier
            0x04, 0x1F, // maxPropId=4, bitmap {0,1,2,3,4}
            0x70, 0x77, 0x06, // type nibbles [Int,Byte,Byte,Byte,Bool]
            0x78, 0x00, 0x00, 0x00, // p0 Int = 120 (Player obj)
            0x37, // p1 Byte = 55 (Player)
            0x03, // p2 Byte = 3 (Autonomous)
            0x3D, // p3 Byte = 61 (LoadoutClientBackendSynchronized)
            0x01, // p4 Bool = true (HideHelmet)
        ];
        assert_eq!(got, want);
    }

    /// Byte-for-byte vs s506 #3523661 (the final-round death): op29
    /// `PlayerDeadStateChange` rides carrier 0x36 on the dead Avatar (obj 124), with
    /// the two packed-stats ULongs at p4/p5 and a cause byte at p6 (the props-0-6
    /// avatar-state-change shape the family shares — proven against the captured
    /// header `0a ff 07 70 77 22 d7`). Supersedes the old bare-NetObjectInfo guess.
    #[test]
    fn player_dead_matches_s506() {
        // s506 #3523661 values: dead obj 124, dead stats 0x000001ec000001ea, other
        // 0x3b86f83000001ea, cause 3.
        let got = player_dead(124, 2_113_123_910_122, 4_289_388_580_159_095_274, 3);
        let want = [
            0xBE, 0x36, // marker + UserMessage carrier
            0x06, 0x7F, // maxPropId=6, bitmap {0,1,2,3,4,5,6}
            0x70, 0x77, 0x22, 0x07, // type nibbles [Int,Byte,Byte,Byte,ULong,ULong,Byte]
            0x7C, 0x00, 0x00, 0x00, // p0 Int = 124 (dead avatar obj)
            0x38, // p1 Byte = 56 (Avatar)
            0x01, // p2 Byte = 1 (Authority)
            0x1D, // p3 Byte = 29 (PlayerDeadStateChange)
            0xEA, 0x01, 0x00, 0x00, 0xEC, 0x01, 0x00, 0x00, // p4 ULong dead stats
            0xEA, 0x01, 0x00, 0x00, 0x30, 0xF8, 0x86, 0x3B, // p5 ULong other stats
            0x03, // p6 Byte = 3 (cause)
        ];
        assert_eq!(got, want, "op29 props 0-6 must byte-match s506 #3523661");
    }

    /// Byte-for-byte vs s506 #3523671 — op48 `MatchPostRoundInfoMsg`, the real retail
    /// match-RESULT message (carrier 0x36 on the Match obj 123). winner=Blank
    /// (1131a037…), loser=Flappety (38c987fd…), matchId 88e9347a…, result code 3.
    /// This frame self-validates (ENet dataLength 339 == BE36+consumed 337).
    #[test]
    fn match_post_round_info_matches_s506() {
        let got = match_post_round_info(
            123,
            "1131a037-716c-49cc-b165-32d8ddc14f49", // winner
            "38c987fd-c42b-4ea6-b869-c8d4c03055f9", // loser
            "88e9347a-f060-40d6-b796-a61b8c4d233e", // matchId
            3,
        );
        // Carrier + structural framing, then every field decoded (the captured frame
        // self-validates: ENet dataLength 339 == BE 36 + the 337-byte NetData).
        assert_eq!(&got[0..2], &[0xBE, 0x36], "marker + UserMessage carrier");
        assert_eq!(got.len(), 339, "op48 frame is 339 bytes (BE 36 + 337-byte NetData)");
        let nd = arena_proto::parse_netdata(&got[2..]);
        assert_eq!(nd.int(0), Some(123), "p0 Match obj id");
        assert_eq!(nd.int(1), Some(54), "p1 Match");
        assert_eq!(nd.int(2), Some(1), "p2 Authority");
        assert_eq!(nd.int(3), Some(48), "p3 MatchPostRoundInfoMsg gmid");
        assert_eq!(nd.int(4), Some(3), "p4 result code = 3");
        let w = "1131a037-716c-49cc-b165-32d8ddc14f49";
        let l = "38c987fd-c42b-4ea6-b869-c8d4c03055f9";
        // Winner repeats at p5/p7/p12/p16, loser at p6/p8/p13 (s506 cross-check).
        for p in [5, 7, 12, 16] {
            assert_eq!(nd.string(p), Some(w), "p{p} = winner char UUID");
        }
        for p in [6, 8, 13] {
            assert_eq!(nd.string(p), Some(l), "p{p} = loser char UUID");
        }
        assert_eq!(nd.string(9), Some(""), "p9 empty");
        assert_eq!(nd.string(10), Some(""), "p10 empty");
        assert_eq!(nd.int(11), Some(1), "p11 Byte 1");
        assert_eq!(nd.props.get(&14), Some(&arena_proto::NetDataValue::Bool(false)), "p14 false");
        assert_eq!(nd.props.get(&15), Some(&arena_proto::NetDataValue::Bool(true)), "p15 true");
        assert_eq!(nd.props.get(&17), Some(&arena_proto::NetDataValue::Bool(false)), "p17 false");
        assert_eq!(nd.string(18), Some("88e9347a-f060-40d6-b796-a61b8c4d233e"), "p18 matchId");
    }

    /// The carrier-`0x36` GameMessageId reader + the combat/non-combat split that
    /// keeps round-transition handshake frames from being resolved as swings.
    #[test]
    fn user_message_gmid_and_noncombat_split() {
        // op61 (the s506 c2s bytes, marker patched to the c2s 0x84 — byte 0 unused).
        let op61 = {
            let mut f = loadout_client_backend_synchronized(120, NetRole::Autonomous, true);
            f[0] = 0x84;
            f
        };
        assert_eq!(user_message_gmid(&op61), Some(61));
        assert!(is_loadout_backend_synchronized(&op61));
        assert!(is_noncombat_user_message(&op61), "op61 is handshake, not a swing");

        // op80 MatchStateChangeAck + op36 PlayerLoadoutReady are non-combat too.
        assert!(is_noncombat_user_message(&match_state_change_ack(119, "StateTimeout")));
        let op36 = {
            let mut w = NetDataWriter::new();
            w.int(0, 120).byte(1, 55).byte(2, 3).byte(3, 36);
            frame(MSGTYPE_USERMESSAGE, w.finish())
        };
        assert!(is_noncombat_user_message(&op36), "PlayerLoadoutReady is handshake");

        // A real combat swing/ability is NOT classified as non-combat.
        // op37 RequestExecuteAbility (real cast) — must fall through to resolution.
        let mut op37 = vec![
            0xBE, 0x36, 0x04, 0x1F, 0x70, 0x77, 0x0A, 0x35, 0x02, 0x00, 0x00, 0x38, 0x03, 0x25,
            0x24, 0x00,
        ];
        op37.extend_from_slice(b"7fc15804-1637-40a9-8dcc-3ea1eb0f778d");
        assert!(!is_noncombat_user_message(&op37), "an ability cast is combat, not handshake");
        // A bare swipe body (no decodable propId 3) is NOT non-combat → resolves as a swing.
        assert_eq!(user_message_gmid(&[0x84, 0x36]), None);
        assert!(!is_noncombat_user_message(&[0x84, 0x36]));
        // Non-0x36 carriers are never user-messages.
        assert_eq!(user_message_gmid(&[0x84, 0x3a, 0x00]), None);
    }

    /// op73 PlayerEmoteStateChange (the s2c emote relay) carries the emoting avatar's
    /// NetObjectInfo, the Emote state-id (28), and the emote id string; readable back
    /// by the c2s `play_emote_id` decoder shape.
    #[test]
    fn player_emote_state_change_structure() {
        let got = player_emote_state_change(124, "emote_taunt");
        assert_eq!(&got[0..2], &[0xBE, 0x36], "marker + UserMessage carrier");
        let nd = arena_proto::parse_netdata(&got[2..]);
        assert_eq!(nd.int(0), Some(124), "p0 emoting avatar obj");
        assert_eq!(nd.int(1), Some(56), "p1 Avatar");
        assert_eq!(nd.int(3), Some(73), "p3 PlayerEmoteStateChange gmid");
        assert_eq!(nd.int(4), Some(28), "p4 stateId = Emote(28)");
        assert_eq!(nd.string(5), Some("emote_taunt"), "p5 emote id");
    }

    /// The c2s PlayEmote (72) / PlayerBlockingStateChange (41) classifiers + their
    /// payload extractors. A PlayEmote's string is read back; a non-emote returns None.
    #[test]
    fn play_emote_and_block_decode() {
        // c2s PlayEmote (72): {0:obj · 1:55 · 2:role · 3:72 · 4:String id}.
        let emote = {
            let mut w = NetDataWriter::new();
            w.int(0, 120).byte(1, 55).byte(2, 3).byte(3, 72).string(4, "emote_wave");
            let mut f = frame(MSGTYPE_USERMESSAGE, w.finish());
            f[0] = 0x84;
            f
        };
        assert!(is_play_emote(&emote));
        assert_eq!(play_emote_id(&emote).as_deref(), Some("emote_wave"));
        assert!(!is_player_blocking_state_change(&emote));

        // c2s PlayerBlockingStateChange (41), Right side (3).
        let block = {
            let mut w = NetDataWriter::new();
            w.int(0, 120).byte(1, 55).byte(2, 3).byte(3, 41).byte(4, 3);
            let mut f = frame(MSGTYPE_USERMESSAGE, w.finish());
            f[0] = 0x84;
            f
        };
        assert!(is_player_blocking_state_change(&block));
        assert_eq!(blocking_active_side(&block), Some(ActiveSide::Right));
        assert!(!is_play_emote(&block));

        // A real swing (no propId 3) is neither.
        assert!(!is_play_emote(&[0x84, 0x36]));
        assert!(!is_player_blocking_state_change(&[0x84, 0x36]));
        assert_eq!(play_emote_id(&[0x84, 0x36]), None);
    }

    /// op72/73 emotes are classified as non-combat (so the resolve fallback never
    /// treats one as a swing); the engine intercepts op72 for the relay before resolve.
    #[test]
    fn emote_is_noncombat() {
        let emote = {
            let mut w = NetDataWriter::new();
            w.int(0, 120).byte(1, 55).byte(2, 3).byte(3, 72).string(4, "x");
            frame(MSGTYPE_USERMESSAGE, w.finish())
        };
        assert!(is_noncombat_user_message(&emote), "PlayEmote (72) is non-combat");
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
