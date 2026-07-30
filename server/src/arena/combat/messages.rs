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

use super::state::{ActiveSide, DamageSource, DamageType, FlowState, MatchState, NetObjectType, NetRole, StatusEffectType};

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
            // Shield / block raise (41) must never reach resolve_swing — belt-and-
            // suspenders alongside the dedicated `is_player_blocking_state_change`
            // early-return in `resolve::on_c2s_input`. If the GMID decode mismatches
            // on a live device, this fallback prevents the block frame from being
            // treated as a weapon swing and dealing damage. [spec bug 3]
            20 | 22 | 36 | 41 | 56 | 57 | 61 | 72 | 73 | 76 | 79 | 80
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

/// `PlayerStatsUpdate` (GMID 65, carrier `0x36`, **ENet channel 1**) — carries the
/// current Health/Stamina/Magicka fractions of an avatar as a packed ULong (same
/// layout as `ReceiveDamage` propId 4/5: `Health|Stamina<<10|Magicka<<20` in the
/// HIGH 32 bits, `seq` in the LOW 32 bits). Emitted after a regen tick, an ability
/// cost deduction, or a DoT/potion stat change so the HUD bars update. [s506 ch1]
///
/// `packed` is the result of `Fighter::packed_stats()`.
pub fn player_stats_update(avatar_net_object_id: i32, packed: u64) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, avatar_net_object_id)
        .byte(1, super::state::NetObjectType::Avatar as u8)
        .byte(2, 1) // role byte (Authority=1 — same as stat_update round-start)
        .byte(3, 65)
        .ulong(4, packed)
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

/// `ChangeCombatStatusEffect` (51) — a status effect (condition / buff) was applied or
/// removed on an actor. Carrier `0x36`, GMID at propId 3. Capture-proven layout
/// (`docs/arena-status-resistance-spec.md` §5.3, 2 337 frames): `{0:Int actorObj ·
/// 1:Byte 56 Avatar · 2:Byte 1 Authority · 3:Byte 51 · 4:Bool apply/remove · 5:Byte
/// StatusEffectType · 6:Float duration · 7:Byte sourceDamageType}`. propId7 = the source
/// `DamageType` (0 for the four elemental conditions; 255/None otherwise).
pub fn change_combat_status_effect(
    actor_net_object_id: i32,
    apply: bool,
    status: StatusEffectType,
    duration: f32,
    source_damage_type: u8,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, actor_net_object_id)
        .byte(1, NetObjectType::Avatar as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::ChangeCombatStatusEffect as u8)
        .bool(4, apply)
        .byte(5, status as u16 as u8)
        .float(6, duration)
        .byte(7, source_damage_type);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// The constant NetData propId 6 of every captured `PlayerChannelingStateChange` —
/// `4` in **all 1 182** de-duplicated prod op53 frames. Semantics unconfirmed (most
/// likely the `PlayerStateChange` base's `stateId`, i.e. the "channelling" actor
/// state); kept verbatim rather than guessed at.
const CHANNELING_STATE_ID: u8 = 4;

/// op53 `PlayerChannelingStateChange` (carrier `0x36`, GameMessageId at NetData
/// propId 3) — the caster's spell/ability CAST (channel) state change. This is what
/// drives the client's cast animation and channelling VFX; with it missing, spells
/// fire with no build-up. Retail sends it immediately after the
/// `PerformExecuteAbility` (38) echo (s127: c2s op37 #954963 → s2c op38 #954965 →
/// s2c op53 #954966).
///
/// **Capture-derived layout** — 1 182 de-duplicated prod frames (2 450 raw), every
/// one of them s2c and carrier `0x36` (NOT `0x35`: that carrier is the net-object
/// property update `MSGTYPE_NETOBJ_UPDATE`, a different family that older notes
/// conflated with op53). propIds and type nibbles are identical in all 1 182:
///
/// `{0:Int avatarObj · 1:Byte 56 Avatar · 2:Byte 1 Authority · 3:Byte 53 ·
///   4:ULong caster packed stats · 5:ULong opponent packed stats ·
///   6:Byte 4 · 7:ByteArray <unmodelled> · 8:Float <time, s> · 9:String abilityUuid}`
///
/// propId 4/5 carry the same packed-pool word as `ReceiveDamage`/`PlayerStatsUpdate`
/// (`Health|Stamina<<10|Magicka<<20` in the hi32, seq in the lo32) — decoding them
/// against the captures tracks the caster's and the opponent's bars draining.
///
/// **Two fields could NOT be resolved from the corpus and are therefore not invented:**
///
/// * **propId 7** — a variable-length blob (6…23 B, 778 distinct values across 1 182
///   frames) whose internal structure did not fall out of the corpus. On the wire it
///   is a `ByteArray` with a **u8** length prefix (proven: with u8 all 1 182 frames
///   parse to exactly their byte length, with u16 none do). `arena_proto`'s
///   `NetDataWriter` now emits that u8 prefix natively — see
///   [`arena_proto::NetDataType::len_prefix_width`] — so this builder round-trips a
///   retail op53 with no post-processing. Production emission passes `state_blob = None`
///   and simply OMITS the property — NetData is a sparse property bag, so the client
///   leaves the field at its default rather than reading a fabricated blob.
///   `Some(..)` exists so the byte-differential tests can rebuild a real captured
///   frame exactly.
/// * **propId 8** — a float, always a multiple of 1/60 s. It is demonstrably **NOT**
///   the shipped `_channelDuration`: that value is rank-invariant per ability
///   (Fireball 0.9, Lightning Bolt 0.5, Poison Cloud 1.3, `cfee0b02…` 1.12) whereas
///   the captured floats spread widely for one ability (Lightning Bolt 0.15…1.27,
///   Fireball 0.03…1.83), and `4be1d681…` — which defines no `_channelDuration` at
///   all — still appears with 0.33/0.35/1.63. The deduped histogram peaks at
///   0.12…0.35 s, which reads like an elapsed/latency offset. We send the caster's
///   own shipped `_channelDuration` because that is the value with the right
///   *meaning* for a cast-animation length; the exact retail semantics stay open.
pub fn player_channeling_state_change(
    caster_avatar_net_object_id: i32,
    caster_packed_stats: u64,
    opponent_packed_stats: u64,
    channel_duration_secs: f32,
    ability_uuid: &str,
    state_blob: Option<&[u8]>,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, caster_avatar_net_object_id)
        .byte(1, NetObjectType::Avatar as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::PlayerChannelingStateChange as u8)
        .ulong(4, caster_packed_stats)
        .ulong(5, opponent_packed_stats)
        .byte(6, CHANNELING_STATE_ID);
    if let Some(blob) = state_blob {
        w.put(7, arena_proto::NetDataValue::ByteArray(blob.to_vec()));
    }
    w.float(8, channel_duration_secs).string(9, ability_uuid);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// op64 `PerformConsumeConsumable` (carrier `0x36`, GameMessageId at NetData propId
/// 3) — the server's authoritative confirmation that a fighter drank its equipped
/// consumable. Sent to BOTH players so each renders the drink animation.
///
/// Capture-derived layout (554 prod frames, all s2c, all carrier `0x36`, all this
/// exact shape): `{0:Int avatarObj · 1:Byte 56 Avatar · 2:Byte 1 Authority ·
/// 3:Byte 64 · 4:String consumableItemUuid}`. The UUIDs resolve against the capture
/// platform's `uuid_labels` to real items (`d826ea12…` = "Potion of Light Healing",
/// `819094ad…` = "Potion of Healing"), and match the UUID the same avatar declared
/// in its `EquipAbilitiesAndConsumables` (56) upload — see
/// [`super::input::parse_equip_consumables`]. Byte-for-byte vs s127 #962751.
pub fn perform_consume_consumable(avatar_net_object_id: i32, consumable_uuid: &str) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, avatar_net_object_id)
        .byte(1, NetObjectType::Avatar as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::PerformConsumeConsumable as u8)
        .string(4, consumable_uuid);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// `DamageNegated` (66) — a Ward/Absorb/Dodge pool fully ate a hit (no damage payload).
/// Carrier `0x36`, GMID at propId 3, on the defending Avatar. A bare NetObjectInfo +
/// GMID signal (the captured op66 carries no further fields — §3.3/§4.5). Emitted
/// INSTEAD of letting the hit reduce HP when a negation pool consumed the whole hit.
pub fn damage_negated(defender_net_object_id: i32) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, defender_net_object_id)
        .byte(1, NetObjectType::Avatar as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::DamageNegated as u8);
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
/// per-round result and to TALLY the running score.
///
/// **Field semantics (dump.cs `MatchPostRoundInfoMessage : MatchInfoMessage`, 589846):**
/// beyond the NetObjectInfo header + gmid, the message carries `RoundNumber` (which
/// round this result is for), `WinnerPlayerId`/`LoserPlayerId` (this round's result),
/// `MatchConceded`/`OpponentDisconnected`/`IsMatchEnded` bools, `MatchWinnerPlayerId`
/// (the OVERALL match winner — only meaningful once `IsMatchEnded`), and a trailing
/// telemetry id. **The client accumulates the score from the per-round Winner/Loser it
/// sees each round, and only closes the match when `IsMatchEnded` is true.**
///
/// **CAPTURE-PINNED 2026-07-30 — and it overturns two earlier claims.**
///
/// The previous comment said op48 "was never captured (0 rows in all prod sessions),
/// so this layout is dump.cs-derived, not byte-pinned". That is wrong: there are
/// **375 s2c op48 frames across 43 sessions**. Decoding all of them gives the exact
/// contract, with no exceptions:
///
/// ```text
///   {0:Int matchObj · 1:Byte 54 Match · 2:Byte 1 Authority · 3:Byte 48
///    · 4:Int 3                      <- CONSTANT, never the live round number
///    · 5,6   :String round-1 winner,loser
///    · 7,8   :String round-2 winner,loser  ("" until round 2 completes)
///    · 9,10  :String round-3 winner,loser  ("" until round 3 completes)
///    · 11:Byte  completedRounds - 1
///    · 12,13 :String most-recent round winner,loser
///    · 14:Bool MatchConceded · 15:Bool IsMatchEnded
///    · 16:String MatchWinnerPlayerId (non-empty iff IsMatchEnded)
///    · 17:Bool OpponentDisconnected · 18:String matchId}
/// ```
///
/// Observed combinations (375 frames): `pairs=1,p11=0,ended=false` ×45;
/// `pairs=2,p11=1,ended=false` ×6 (a 1-1 match going to round 3);
/// `pairs=2,p11=1,ended=true` ×271 (a 2-0 sweep); `pairs=3,p11=2,ended=true` ×53.
/// propId 11 is ALWAYS `pairs-1`, and propId 4 is ALWAYS 3.
///
/// So the message is **cumulative**: the client reads the whole round-by-round array
/// and tallies the score from it. That is why the earlier "bug-1 fix" made things
/// worse rather than better — it started sending the live round number at propId 4
/// (retail always sends 3) while still emitting a single result duplicated into the
/// round-2 slot with `11` hardcoded to 1. After round 1 the client therefore saw two
/// recorded rounds under a round number it did not expect; the reported symptoms were
/// "0-0 after round 1, then 3-0 or 0-3, and the third round labelled the 4th".
///
/// The `RoundInfo[]` the base class serializes IS representable after all — it is just
/// these three fixed (winner, loser) slot pairs.
#[allow(clippy::too_many_arguments)]
pub fn match_post_round_info(
    match_net_object_id: i32,
    // Cumulative per-round results, round 1 first: (winner_uuid, loser_uuid).
    round_results: &[(String, String)],
    match_id: &str,
    is_match_ended: bool,
) -> Vec<u8> {
    // The client tallies the displayed score from this cumulative array, so every
    // completed round must be present, in order. Slots are (5,6), (7,8), (9,10) —
    // three, because the match is best-of-3 — and unused slots are empty strings.
    let slot = |i: usize| -> (&str, &str) {
        round_results
            .get(i)
            .map(|(w, l)| (w.as_str(), l.as_str()))
            .unwrap_or(("", ""))
    };
    let (w1, l1) = slot(0);
    let (w2, l2) = slot(1);
    let (w3, l3) = slot(2);

    // propId 11 = index of the LAST filled slot (= completed rounds - 1). Exact in all
    // 375 captured frames: 1 round→0, 2→1, 3→2.
    let last_index = round_results.len().saturating_sub(1) as u8;

    // The most recent round's winner/loser is repeated at 12/13 in every capture.
    let (latest_w, latest_l) = round_results
        .last()
        .map(|(w, l)| (w.as_str(), l.as_str()))
        .unwrap_or(("", ""));

    // MatchWinnerPlayerId names the OVERALL winner and is non-empty in exactly the
    // frames where IsMatchEnded is true (271 + 53 captured frames, no exceptions).
    let match_winner = if is_match_ended { latest_w } else { "" };

    let mut w = NetDataWriter::new();
    w.int(0, match_net_object_id)
        .byte(1, NetObjectType::Match as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::MatchPostRoundInfoMsg as u8)
        // RoundNumber is a CONSTANT 3, not this round's index — it is 3 in all 375
        // captured frames, including mid-match ones. Sending the live round number
        // here is a deviation from retail (see the doc comment).
        .int(4, ROUND_NUMBER_CONST)
        .string(5, w1)
        .string(6, l1)
        .string(7, w2)
        .string(8, l2)
        .string(9, w3)
        .string(10, l3)
        .byte(11, last_index)
        .string(12, latest_w)
        .string(13, latest_l)
        .bool(14, false) // MatchConceded
        .bool(15, is_match_ended) // IsMatchEnded
        .string(16, match_winner) // MatchWinnerPlayerId — empty until the match ends
        .bool(17, false) // OpponentDisconnected
        .string(18, match_id);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// `RoundNumber` (op48 propId 4) is a fixed 3 on the wire — capture-pinned, 375/375.
pub const ROUND_NUMBER_CONST: i32 = 3;

/// The arena GOLD (soft) currency UUID — `reward.currencies[<this>]` is the per-match
/// gold the victory card animates. CONSTANT across all captured sessions (s506/503/504/
/// 486/167/127). [`docs/arena-match-end-spec.md` §3.1]
pub const ARENA_GOLD_CURRENCY_UUID: &str = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2";

/// `MatchEndMatchMsg` (49) — **the match-END message that drives the client's
/// victory/results CARD.** Carrier `0x36`, GMID at NetData propId 3, on the **Match**
/// net-object. Header MIRRORS op48 (`match_post_round_info`): the winner/loser char-UUID
/// quartet + the result_code at p4; op49 ADDS the per-recipient `ResultsJSON` at propId
/// 13 + the p14/p15/p16 trailer. ENet auto-fragments the ~4 KB body on ch4 (the same
/// channel as the op54 profile).
///
/// **Correction (`docs/arena-match-end-spec.md`):** op49 IS sent at match-end (the old
/// stub's "NOT sent by retail / rides 0xc2/0xc6" was WRONG — that was a misread of the
/// ENet fragment-frame header; the real carrier is `0x36` with GMID 49 at propId 3,
/// capture-proven in 6 sessions by reassembling the fragmented frame). [s506 #3523709]
///
/// Layout (byte-exact, s506 #3523709): `{0:Int matchObj · 1:Byte 54 Match · 2:Byte 1
/// Authority · 3:Byte 49 · 4:Int result_code · 5/7/11:String winner · 6/8/12:String
/// loser · 9/10:String "" · 13:String ResultsJSON · 14:Bool false · 15:Byte 0 · 16:Int}`.
#[allow(clippy::too_many_arguments)]
pub fn match_end_match(
    match_net_object_id: i32,
    winner_char_uuid: &str,
    loser_char_uuid: &str,
    result_code: i32,
    results_json: &str,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, match_net_object_id)
        .byte(1, NetObjectType::Match as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::MatchEndMatchMsg as u8) // 49
        .int(4, result_code)
        .string(5, winner_char_uuid)
        .string(6, loser_char_uuid)
        .string(7, winner_char_uuid)
        .string(8, loser_char_uuid)
        .string(9, "")
        .string(10, "")
        .string(11, winner_char_uuid)
        .string(12, loser_char_uuid)
        .string(13, results_json)
        .bool(14, false)
        .byte(15, 0)
        .int(16, 0); // s506=757; a small per-match int, not load-bearing — 0 is fine
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// The per-match reward + post-match PvP deltas for ONE recipient (the inputs to
/// [`results_json`]). The victory card animates `gold` + `character_xp`; the trophy/rank
/// deltas come from the post-match `pvp_trophies` / `challenge_rank` the client diffs
/// against its pre-match values. [`docs/arena-match-end-spec.md` §3]
#[derive(Debug, Clone)]
pub struct MatchEndReward {
    /// Gold granted this match (`reward.currencies[ARENA_GOLD_CURRENCY_UUID]`).
    pub gold: i64,
    /// XP granted this match (`reward.characterXp`).
    pub character_xp: i64,
    /// The recipient's gold WALLET balance AFTER crediting (`wallet[].balance`).
    pub wallet_gold: i64,
    /// The recipient's VISIBLE trophy count, post-match (`character.pvpTrophies`).
    pub pvp_trophies: i64,
    /// The matchmaking-internal trophies, post-match (`character.matchmakingPvpTrophies`).
    ///
    /// The 108 reassembled retail cards show this is the season **high-water mark**
    /// (monotone non-decreasing, `= max(pvpTrophies)`), and that it — not the live
    /// `pvpTrophies` — is what the arena ladder promotes on.
    pub matchmaking_pvp_trophies: i64,
    /// The recipient's challenge-season rank, post-match (`character.challengeSeason.rank`).
    pub challenge_rank: i64,
    /// `character.pvpChestMeter`, post-match. Counts **rounds won** (not matches)
    /// and wraps at `pvp_match_rewards.chest_meter_capacity` = 8. Capture-proven by
    /// diffing consecutive cards against `numberPvpMatchPlayed`.
    pub pvp_chest_meter: i64,
    /// `character.pvpWinningStreak`, post-match. Positive counts consecutive wins,
    /// negative consecutive losses; the sign is the card's own win/loss marker.
    pub pvp_winning_streak: i64,
    /// `character.numberPvpMatchPlayed`, post-match (pre-match + 1).
    pub number_pvp_match_played: i64,
    /// `character.highestArenaReached` — the ladder arena for the post-match
    /// high-water mark.
    pub highest_arena_reached: u64,
    /// `character.highestLevelArenaReached` — the ladder level within that arena.
    pub highest_level_arena_reached: u64,
    /// The `rewardNewLevelArena` block. `{}` on every ordinary match; populated
    /// only when this match crossed one or more ladder `required_trophy_count`
    /// thresholds. See `arena_ladder::PromotionRewards` for the three retail
    /// examples the shape is taken from.
    pub reward_new_level_arena: serde_json::Value,
}

impl Default for MatchEndReward {
    fn default() -> Self {
        MatchEndReward {
            gold: 0,
            character_xp: 0,
            wallet_gold: 0,
            pvp_trophies: 0,
            matchmaking_pvp_trophies: 0,
            challenge_rank: 1,
            pvp_chest_meter: 0,
            pvp_winning_streak: 0,
            number_pvp_match_played: 0,
            highest_arena_reached: 1,
            highest_level_arena_reached: 1,
            // NOT `Value::Null` — the card's field is an empty OBJECT when there
            // was no promotion, which is what every non-promotion retail card has.
            reward_new_level_arena: serde_json::json!({}),
        }
    }
}

/// Build the op49 `ResultsJSON` (propId 13) — the victory-card payload
/// (`docs/arena-match-end-spec.md` §3). PER-RECIPIENT: `characterId`,
/// `reward.{currencies, characterXp}`, the recipient `character` snapshot (with the
/// post-match `pvpTrophies`/`matchmakingPvpTrophies`/`challengeSeason.rank` overlaid),
/// `inventory` (the op54 equipped snapshot), and the credited `wallet`.
///
/// `character_json` / `equipped_items_json` are the recipient's op54 PROFILE blobs (the
/// same source the transfer/profile path uses — `loadout.profile_character_json` /
/// `profile_equipped_json`); empty/unparseable falls back to a minimal `{id,name}`
/// character so a starter/bot still produces a valid card. `request_index` is the
/// player's REST request sequence (echo/idempotency; 0 is fine).
pub fn results_json(
    recipient_char_uuid: &str,
    character_json: &str,
    equipped_items_json: &str,
    reward: &MatchEndReward,
    request_index: i64,
) -> String {
    use serde_json::{json, Value};

    // Start from the recipient's op54 character record; overlay the post-match PvP
    // fields the card reads (the rest of the snapshot is preserved verbatim).
    let mut character: Value = serde_json::from_str(character_json).unwrap_or_else(|_| {
        json!({ "id": recipient_char_uuid, "name": "" })
    });
    if let Some(obj) = character.as_object_mut() {
        obj.insert("id".into(), json!(recipient_char_uuid));
        obj.insert("pvpTrophies".into(), json!(reward.pvp_trophies));
        obj.insert("matchmakingPvpTrophies".into(), json!(reward.matchmaking_pvp_trophies));
        // The rest of the post-match PvP block. Retail's card carries all of these
        // and the arena menu reads them straight back out of the snapshot, so
        // leaving them at their stale pre-match values made the chest meter and the
        // ladder position visibly rewind the moment the card appeared.
        obj.insert("pvpChestMeter".into(), json!(reward.pvp_chest_meter));
        obj.insert("pvpWinningStreak".into(), json!(reward.pvp_winning_streak));
        obj.insert("numberPvpMatchPlayed".into(), json!(reward.number_pvp_match_played));
        obj.insert("highestArenaReached".into(), json!(reward.highest_arena_reached));
        obj.insert(
            "highestLevelArenaReached".into(),
            json!(reward.highest_level_arena_reached),
        );
        // challengeSeason.rank — create/overlay just the rank (the card reads it for the
        // rank delta); preserve any other season fields already present.
        let mut season = obj.get("challengeSeason").cloned().unwrap_or_else(|| json!({}));
        if let Some(s) = season.as_object_mut() {
            s.insert("rank".into(), json!(reward.challenge_rank));
        } else {
            season = json!({ "rank": reward.challenge_rank });
        }
        obj.insert("challengeSeason".into(), season);
    }

    // The inventory snapshot (the op54 equipped-items blob the menu re-syncs from).
    let inventory: Value = serde_json::from_str::<Value>(equipped_items_json)
        .map(|loadout| json!({ "loadout": loadout }))
        .unwrap_or_else(|_| json!({}));

    json!({
        "characterId": recipient_char_uuid,
        "character": character,
        "reward": {
            "currencies": { ARENA_GOLD_CURRENCY_UUID: reward.gold },
            "characterXp": reward.character_xp,
        },
        "rewardNewLevelArena": reward.reward_new_level_arena,
        "currentRequestIndex": request_index,
        "inventory": inventory,
        "wallet": [ { "currencyId": ARENA_GOLD_CURRENCY_UUID, "balance": reward.wallet_gold } ],
    })
    .to_string()
}

/// `PlayerChargingStateChange` (45) — the s2c "this actor is charging a swing"
/// broadcast. **This is the charge/combo circle.**
///
/// CAPTURE-PINNED 2026-07-30 from 13,060 decoded frames across 46 sessions, one
/// prop-set, all s2c. We were sending it ZERO times, which is why a plain swing
/// showed no circle while tapping an ability did (an ability path incidentally put
/// the client into a charge state).
///
/// It is one member of the PlayerStateChange family (39/41/43/44/45/52), which all
/// share this frame:
///
/// ```text
///   {0:Int actorObj · 1:Byte 56 Avatar · 2:Byte 1 Authority · 3:Byte gmid
///    · 4,5:Long packed stats · 6:Byte ActorStateType · 7:ByteArray stateHistory
///    · 8:Single timeInState · 9:Byte ActiveSide}
/// ```
///
/// propId 6 is the state id and is CONSTANT 2 for charging in all 13,060 frames
/// (cf. Blocking=1, Recovery=16, FollowThrough=17, AutoAttack=19). propId 9 is the
/// side and only ever holds 2 or 3 — Left/Right — never Middle, which fits: a
/// charge always has a swipe direction.
///
/// NOT pinned per-instance, and deliberately conservative:
///   * propId 7 `stateHistory` is a ByteArray whose contents vary per frame; we send
///     it EMPTY. If the client turns out to need real history, that is the next
///     thing to pin (correlate consecutive frames within one session).
///   * propId 8 is the time already spent in the state; 0.0 is correct at entry
///     (0.0 is also the single most common captured value).
pub fn player_charging_state_change(
    actor_net_object_id: i32,
    own_packed_stats: u64,
    opponent_packed_stats: u64,
    side: ActiveSide,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, actor_net_object_id)
        .byte(1, NetObjectType::Avatar as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::PlayerChargingStateChange as u8)
        .long(4, own_packed_stats as i64)
        .long(5, opponent_packed_stats as i64)
        .byte(6, ACTOR_STATE_CHARGING)
        .string(7, "")
        .float(8, 0.0)
        .byte(9, side as u8);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// `ActorStateType` for charging — propId 6 of gmid 45, constant across all 13,060
/// captured frames.
pub const ACTOR_STATE_CHARGING: u8 = 2;

/// The s2c relay of a client's `PlayEmote` (72) — sent back as `PlayEmote` (72).
///
/// CAPTURE-PINNED 2026-07-30, and it corrects what was here before. The server does
/// NOT answer with `PlayerEmoteStateChange` (73): across 264,302 decoded retail
/// frames, gmid 73 appears ZERO times, while gmid 72 appears 1,230 times c2s and
/// 2,014 times s2c over 45 sessions. Retail echoes the SAME message id back, and
/// the s2c frame is shape-identical to the c2s one — every one of them 62 bytes
/// with the single prop set {0,1,2,3,4}:
///
///   marker 0xBE · carrier 0x36 · {0:Int emoterObj · 1:Byte 56 Avatar
///                                 · 2:Byte 3 Autonomous · 3:Byte 72 PlayEmote
///                                 · 4:String emoteId}
///
/// `emoteId` is a 36-char UUID (e.g. "a654c0e8-bef2-4d9e-8384-642a68eba019"), not a
/// symbolic name. There is NO ActorStateType/stateId property, and the string sits
/// at propId 4 — the previous shape put a stateId at 4 and the string at 5, under a
/// gmid the client never receives from retail, which is why pressing an emote
/// animated nothing for either player.
///
/// Note prop2 is Autonomous (3), not Authority (1): every captured s2c relay uses 3.
///
/// The old comment claimed the raw c2s PlayEmote frame was "not byte-decodable from
/// the retained ENet-framed captures". It is — the NetData frame begins at offset 10,
/// after the ENet header; parsing from there decodes cleanly.
///
/// Relayed to the emoter (so its own animation plays — the server is authoritative
/// over actor state) AND to the opponent (so it shows on the other screen).
/// `dump.cs:588944` (`PlayEmoteMessage`, single `string _emoteId`).
pub fn play_emote_relay(emoting_avatar_net_object_id: i32, emote_id: &str) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, emoting_avatar_net_object_id)
        .byte(1, NetObjectType::Avatar as u8) // 56
        .byte(2, NetRole::Autonomous as u8) // 3 — NOT Authority; see below
        .byte(3, GameMessageId::PlayEmote as u8) // 72 — the SAME id the client sent
        .string(4, emote_id);
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
    // CAPTURE-PINNED: the emoteId is a String at propId 4 (a 36-char UUID). All
    // 1,230 captured c2s PlayEmote frames carry exactly the prop set {0,1,2,3,4}.
    if let Some(s) = nd.string(4) {
        return Some(s.to_string());
    }
    // Fall back to the first string above the GameMessageId, then to empty — a
    // PlayEmote we can't fully decode should still relay rather than vanish.
    let mut keys: Vec<&u8> = nd.props.keys().filter(|k| **k > 3).collect();
    keys.sort();
    for k in keys {
        if let Some(s) = nd.string(*k) {
            return Some(s.to_string());
        }
    }
    Some(String::new())
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
    active_side_of_state_change(user_data)
}

/// Read `ActiveSide` from any member of the PlayerStateChange family — propId **9**.
///
/// CAPTURE-PINNED 2026-07-30. The family (39, 41, 43, 44, 45, 52) shares one frame:
///
/// ```text
///   {0:Int actorObj · 1:Byte 56 Avatar · 2:Byte 1 Authority · 3:Byte gmid
///    · 4,5:Long packed stats · 6:Byte ActorStateType · 7:ByteArray stateHistory
///    · 8:Single timeInState · 9:Byte ActiveSide}   (+ per-message tail at 10)
/// ```
///
/// propId 6 is the STATE ID and is constant per message type — Blocking=1,
/// Charging=2, Recovery=16, FollowThrough=17, AutoAttack=19 — while gmid 39, the
/// generic member, varies it (0/5/13/27/28; 13=Paralyzed, 28=Emote). propId 9 is
/// the side: constant **1 (Middle)** across all 6,643 captured op41 blocks, and
/// {2,3} = Left/Right across the swing family (43/44/45/52). Only 1/2/3 ever
/// appear, which is exactly `ActiveSide` minus `None`.
///
/// THE BUG THIS REPLACES: the previous reader took "the first int-typed property
/// above 3", which is **propId 4 — a packed-stats u64**. That never matches 1/2/3,
/// so it fell through to `ActiveSide::None` for EVERY block ever received. Block
/// side was therefore never actually decoded.
/// CAVEAT on direction: the pinned shape above is the SERVER's broadcast — all
/// 6,643 captured op41 frames are s2c and retail shows **zero** c2s op41 (the
/// client signals a guard some other way, most likely gmid 46
/// `PlayerCombatInputActivate`, 17,817 c2s frames). So propId 9 is authoritative
/// for s2c, and unverified for anything inbound. Hence: prefer propId 9, and for a
/// frame that lacks it fall back to a scan — but one that SKIPS propIds 4 and 5,
/// the packed-stat words whose accidental capture was the original bug.
pub fn active_side_of_state_change(user_data: &[u8]) -> Option<ActiveSide> {
    let nd = arena_proto::parse_netdata(user_data.get(2..)?);
    let to_side = |v: i64| match v {
        1 => Some(ActiveSide::Middle),
        2 => Some(ActiveSide::Left),
        3 => Some(ActiveSide::Right),
        _ => None,
    };
    if let Some(side) = nd.int(9).and_then(to_side) {
        return Some(side);
    }
    // Fallback for un-pinned (inbound / short) shapes: the first property above 3
    // whose value is actually in range. An ActiveSide is only ever 1..=3, so the
    // packed-stat u64s at 4/5 are excluded BY VALUE and the scan simply continues.
    // The original bug was not that it looked at propId 4 — it was that it
    // *returned* `_ => None` on the first non-matching value instead of skipping it.
    let mut keys: Vec<u8> = nd.props.keys().copied().filter(|k| *k > 3).collect();
    keys.sort_unstable();
    keys.into_iter().find_map(|k| nd.int(k).and_then(to_side))
}

/// The `ActorStateType` a PlayerStateChange-family frame carries (propId 6).
pub fn state_change_actor_state(user_data: &[u8]) -> Option<i64> {
    arena_proto::parse_netdata(user_data.get(2..)?).int(6)
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

    /// op48 `MatchPostRoundInfoMsg` — pinned against ALL 375 captured s2c frames.
    ///
    /// The two things this guards, both of which were wrong before and produced the
    /// reported "0-0 after round 1, then 3-0, and the third round labelled the 4th":
    ///   1. propId 4 is a CONSTANT 3 (375/375), never the live round number.
    ///   2. the message is CUMULATIVE — slots (5,6),(7,8),(9,10) hold rounds 1..3 in
    ///      order and propId 11 is `completedRounds - 1`.
    #[test]
    fn match_post_round_info_is_cumulative_and_pins_round_number() {
        let a = "1131a037-716c-49cc-b165-32d8ddc14f49"; // player A char uuid
        let b = "38c987fd-c42b-4ea6-b869-c8d4c03055f9"; // player B char uuid
        let mid = "88e9347a-f060-40d6-b796-a61b8c4d233e";
        let pair = |w: &str, l: &str| (w.to_string(), l.to_string());

        // ---- after round 1 (A won), match still live ----
        let r1 = match_post_round_info(123, &[pair(a, b)], mid, false);
        assert_eq!(&r1[0..2], &[0xBE, 0x36], "marker + UserMessage carrier");
        let nd = arena_proto::parse_netdata(&r1[2..]);
        assert_eq!(nd.int(0), Some(123), "p0 Match obj id");
        assert_eq!(nd.int(1), Some(54), "p1 Match");
        assert_eq!(nd.int(2), Some(1), "p2 Authority");
        assert_eq!(nd.int(3), Some(48), "p3 gmid");
        assert_eq!(nd.int(4), Some(3), "p4 is a CONSTANT 3 in every captured frame");
        assert_eq!(nd.string(5), Some(a), "p5 round-1 winner");
        assert_eq!(nd.string(6), Some(b), "p6 round-1 loser");
        assert_eq!(nd.string(7), Some(""), "p7 round-2 slot still empty");
        assert_eq!(nd.string(8), Some(""), "p8 round-2 slot still empty");
        assert_eq!(nd.string(9), Some(""), "p9 round-3 slot empty");
        assert_eq!(nd.string(10), Some(""), "p10 round-3 slot empty");
        assert_eq!(nd.int(11), Some(0), "p11 = completedRounds-1 = 0");
        assert_eq!(nd.string(12), Some(a), "p12 latest winner");
        assert_eq!(nd.string(13), Some(b), "p13 latest loser");
        assert_eq!(nd.props.get(&15), Some(&arena_proto::NetDataValue::Bool(false)), "p15 not ended");
        assert_eq!(nd.string(16), Some(""), "p16 empty until the match ends");
        assert_eq!(nd.string(18), Some(mid), "p18 matchId");

        // ---- after round 2, B won it: 1-1, match goes to round 3 (captured ×6) ----
        let r2 = match_post_round_info(123, &[pair(a, b), pair(b, a)], mid, false);
        let nd2 = arena_proto::parse_netdata(&r2[2..]);
        assert_eq!(nd2.int(4), Some(3), "p4 still 3");
        assert_eq!(nd2.string(5), Some(a), "round 1 preserved");
        assert_eq!(nd2.string(7), Some(b), "round 2 winner in the second slot");
        assert_eq!(nd2.string(8), Some(a), "round 2 loser in the second slot");
        assert_eq!(nd2.int(11), Some(1), "p11 = 1");
        assert_eq!(nd2.string(12), Some(b), "p12 tracks the LATEST round");
        assert_eq!(nd2.props.get(&15), Some(&arena_proto::NetDataValue::Bool(false)));

        // ---- 2-0 sweep: ends at round 2 (the most common captured frame, ×271) ----
        let sweep = match_post_round_info(123, &[pair(a, b), pair(a, b)], mid, true);
        let nds = arena_proto::parse_netdata(&sweep[2..]);
        assert_eq!(nds.int(11), Some(1), "p11 = 1 for a two-round match");
        assert_eq!(nds.props.get(&15), Some(&arena_proto::NetDataValue::Bool(true)), "p15 ended");
        assert_eq!(nds.string(16), Some(a), "p16 = overall winner once ended");

        // ---- went the distance: 3 rounds (captured ×53) ----
        let full = match_post_round_info(123, &[pair(a, b), pair(b, a), pair(a, b)], mid, true);
        let ndl = arena_proto::parse_netdata(&full[2..]);
        assert_eq!(ndl.int(4), Some(3), "p4 constant 3");
        assert_eq!(ndl.string(9), Some(a), "round 3 winner fills the third slot");
        assert_eq!(ndl.string(10), Some(b), "round 3 loser fills the third slot");
        assert_eq!(ndl.int(11), Some(2), "p11 = 2 for a three-round match");
        assert_eq!(ndl.string(16), Some(a), "overall winner");
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

    /// The s2c emote relay must be byte-shaped like RETAIL's, which echoes gmid 72
    /// — not gmid 73. Pinned against the capture corpus (2026-07-30): gmid 73 occurs
    /// 0 times in 264,302 decoded frames; gmid 72 occurs 2,014 times s2c across 45
    /// sessions, always with the prop set {0,1,2,3,4} and the emoteId String at 4.
    ///
    /// This is the regression guard for "pressing an emote animates nothing": the
    /// previous shape sent gmid 73 with a stateId at 4 and the string at 5, which the
    /// client never receives from retail and therefore ignored.
    #[test]
    fn emote_relay_matches_retail_shape() {
        let emote_uuid = "a654c0e8-bef2-4d9e-8384-642a68eba019";
        let got = play_emote_relay(565, emote_uuid);
        assert_eq!(&got[0..2], &[0xBE, 0x36], "marker + UserMessage carrier");
        let nd = arena_proto::parse_netdata(&got[2..]);
        assert_eq!(nd.int(0), Some(565), "p0 emoter obj");
        assert_eq!(nd.int(1), Some(56), "p1 Avatar");
        assert_eq!(nd.int(2), Some(3), "p2 Autonomous — retail never uses Authority here");
        assert_eq!(nd.int(3), Some(72), "p3 must be PlayEmote(72), NOT 73");
        assert_eq!(nd.string(4), Some(emote_uuid), "p4 emoteId (a UUID)");
        assert_eq!(nd.int(5), None, "no stateId/extra prop — retail stops at 4");

        // The relay is shape-identical to a client frame, so our own c2s decoder
        // must read it back. (Retail's s2c frames are byte-identical in structure.)
        assert!(is_play_emote(&got));
        assert_eq!(play_emote_id(&got).as_deref(), Some(emote_uuid));
    }

    /// A real captured c2s PlayEmote decodes to the pinned prop set. Bytes are the
    /// NetData frame from session 127 (offset 10 onward, past the ENet header).
    #[test]
    fn decodes_a_real_captured_play_emote() {
        let mut f = vec![0xBEu8, 0x36];
        f.extend_from_slice(&hex_bytes(
            "041F70770A35020000380348240061363534633065382D626566322D346439652D383338342D363432613638656261303139",
        ));
        assert!(is_play_emote(&f), "captured frame must classify as PlayEmote");
        assert_eq!(
            play_emote_id(&f).as_deref(),
            Some("a654c0e8-bef2-4d9e-8384-642a68eba019"),
            "emoteId is the 36-char UUID at propId 4"
        );
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// ActiveSide lives at propId 9, capture-pinned across the whole
    /// PlayerStateChange family. Regression for a reader that scanned "first int
    /// prop > 3" and so returned the packed-stats u64 at propId 4 — which matches
    /// no ActiveSide arm, making EVERY block decode as `None`.
    #[test]
    fn active_side_reads_prop9_not_the_packed_stats() {
        // A realistic op41: propIds 4/5 are huge packed-stat words, 6 is the state
        // id (Blocking=1), 9 is the side. Values taken from the captured shape.
        let build = |side: u8| {
            let mut w = NetDataWriter::new();
            w.int(0, 312)
                .byte(1, NetObjectType::Avatar as u8)
                .byte(2, NetRole::Authority as u8)
                .byte(3, GameMessageId::PlayerBlockingStateChange as u8)
                .long(4, 1_078_416_444_930_130_123_i64)
                .long(5, 4_530_621_220_839_751_883_i64)
                .byte(6, 1) // ActorStateType Blocking — NOT the side
                .string(7, "0300001c0001")
                .float(8, 0.533_377_3)
                .byte(9, side);
            let mut f = frame(MSGTYPE_USERMESSAGE, w.finish());
            f[0] = 0x84;
            f
        };

        // The captured constant: every one of the 6,643 op41 frames has side = 1.
        assert_eq!(blocking_active_side(&build(1)), Some(ActiveSide::Middle));
        assert_eq!(blocking_active_side(&build(2)), Some(ActiveSide::Left));
        assert_eq!(blocking_active_side(&build(3)), Some(ActiveSide::Right));

        // The bug: prop 4 is a packed-stats word and must never be read as a side.
        let nd = arena_proto::parse_netdata(&build(2)[2..]);
        assert!(
            nd.int(4).unwrap() > 1_000_000,
            "prop4 is a packed-stats word — the old reader took THIS as the side"
        );
        assert_eq!(nd.int(6), Some(1), "prop6 is the state id, not the side");

        // The state id is exposed separately now.
        assert_eq!(state_change_actor_state(&build(2)), Some(1));
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

    /// op49 `MatchEndMatchMsg` header byte-shape vs s506 #3523709 (`docs/arena-match-end-spec.md`
    /// §2): carrier 0x36 on the Match obj, GMID 49 at propId 3, the winner/loser UUID
    /// quartet (p5/p7/p11 winner, p6/p8/p12 loser), result_code at p4, the ResultsJSON at
    /// p13, and the p14/p15/p16 trailer. The JSON body itself is per-recipient (not
    /// asserted byte-for-byte). This is the op49-IS-SENT correction (the stub was wrong).
    #[test]
    fn match_end_match_matches_s506() {
        let winner = "1131a037-716c-49cc-b165-32d8ddc14f49";
        let loser = "38c987fd-c42b-4ea6-b869-c8d4c03055f9";
        let rj = r#"{"characterId":"38c987fd-c42b-4ea6-b869-c8d4c03055f9","reward":{"currencies":{"f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2":4047},"characterXp":280}}"#;
        let got = match_end_match(123, winner, loser, 3, rj);

        assert_eq!(&got[0..2], &[0xBE, 0x36], "carrier 0x36 (NOT 0xc2/0xc6 — that was a fragment-header misread)");
        let nd = arena_proto::parse_netdata(&got[2..]);
        assert_eq!(nd.int(0), Some(123), "p0 Match obj id");
        assert_eq!(nd.int(1), Some(54), "p1 Match");
        assert_eq!(nd.int(2), Some(1), "p2 Authority");
        assert_eq!(nd.int(3), Some(49), "p3 MatchEndMatchMsg gmid (op49 IS sent)");
        assert_eq!(nd.int(4), Some(3), "p4 result code = 3");
        for p in [5, 7, 11] {
            assert_eq!(nd.string(p), Some(winner), "p{p} = winner char UUID");
        }
        for p in [6, 8, 12] {
            assert_eq!(nd.string(p), Some(loser), "p{p} = loser char UUID");
        }
        assert_eq!(nd.string(9), Some(""), "p9 empty");
        assert_eq!(nd.string(10), Some(""), "p10 empty");
        assert_eq!(nd.string(13), Some(rj), "p13 = the ResultsJSON");
        assert_eq!(nd.props.get(&14), Some(&arena_proto::NetDataValue::Bool(false)), "p14 false");
        assert_eq!(nd.int(15), Some(0), "p15 Byte 0");
        assert_eq!(nd.int(16), Some(0), "p16 Int (s506=757; 0 is fine — not load-bearing)");

        // It routes on ENet ch4 (the big fragmented channel) like the op54 profile.
        assert_eq!(retail_channel(&got), 4, "op49 → ch4 (fragmented, like op54)");
    }

    /// The op49 `ResultsJSON` (propId 13) carries the victory-card fields the client
    /// reads (`docs/arena-match-end-spec.md` §3): `characterId`, the gold currency reward
    /// + characterXp, the credited wallet, and the post-match
    /// pvpTrophies/matchmakingPvpTrophies/challengeSeason.rank overlaid on the char.
    #[test]
    fn results_json_has_card_fields() {
        let reward = MatchEndReward {
            gold: 4047,
            character_xp: 280,
            wallet_gold: 65_039_050,
            pvp_trophies: 755,
            matchmaking_pvp_trophies: 817,
            challenge_rank: 1,
            ..Default::default()
        };
        let char_json = r#"{"id":"old","name":"Flappety","level":86,"experience":291458}"#;
        let equipped = r#"{"equippedItems":{}}"#;
        let out = results_json("38c987fd-c42b-4ea6-b869-c8d4c03055f9", char_json, equipped, &reward, 789104);
        let v: serde_json::Value = serde_json::from_str(&out).expect("ResultsJSON must parse");

        assert_eq!(v["characterId"], "38c987fd-c42b-4ea6-b869-c8d4c03055f9");
        // reward.currencies[<gold uuid>] == gold; reward.characterXp == xp.
        assert_eq!(v["reward"]["currencies"][ARENA_GOLD_CURRENCY_UUID], 4047);
        assert_eq!(v["reward"]["characterXp"], 280);
        // wallet credited with the gold currency.
        assert_eq!(v["wallet"][0]["currencyId"], ARENA_GOLD_CURRENCY_UUID);
        assert_eq!(v["wallet"][0]["balance"], 65_039_050i64);
        // character snapshot: id replaced + post-match PvP fields overlaid; name preserved.
        assert_eq!(v["character"]["id"], "38c987fd-c42b-4ea6-b869-c8d4c03055f9");
        assert_eq!(v["character"]["name"], "Flappety");
        assert_eq!(v["character"]["pvpTrophies"], 755);
        assert_eq!(v["character"]["matchmakingPvpTrophies"], 817);
        assert_eq!(v["character"]["challengeSeason"]["rank"], 1);
        // rewardNewLevelArena empty (no promotion); currentRequestIndex echoed.
        assert_eq!(v["rewardNewLevelArena"], serde_json::json!({}));
        assert_eq!(v["currentRequestIndex"], 789104);
    }

    /// The rest of the post-match PvP block (Phase 5.2). Retail's cards carry
    /// `pvpChestMeter` / `pvpWinningStreak` / `numberPvpMatchPlayed` /
    /// `highestArenaReached` / `highestLevelArenaReached`, and the arena menu reads
    /// them back out of this snapshot.
    ///
    /// The values below are Flappety's real card from prod session **s615 at
    /// 2026-06-27 21:18:21** — the winner half of the two-sided s615/s616 pair.
    #[test]
    fn results_json_carries_the_full_post_match_pvp_block() {
        let reward = MatchEndReward {
            gold: 14961,
            character_xp: 691,
            wallet_gold: 65_808_256,
            pvp_trophies: 773,
            matchmaking_pvp_trophies: 847,
            challenge_rank: 1,
            pvp_chest_meter: 7,
            pvp_winning_streak: 1,
            number_pvp_match_played: 138,
            highest_arena_reached: 2,
            highest_level_arena_reached: 7,
            reward_new_level_arena: serde_json::json!({}),
        };
        let char_json = r#"{"id":"old","name":"Flappety","level":86,"pvpChestMeter":5}"#;
        let out = results_json(
            "38c987fd-c42b-4ea6-b869-c8d4c03055f9",
            char_json,
            "{}",
            &reward,
            789416,
        );
        let v: serde_json::Value = serde_json::from_str(&out).expect("ResultsJSON must parse");

        assert_eq!(v["reward"]["currencies"][ARENA_GOLD_CURRENCY_UUID], 14961);
        assert_eq!(v["reward"]["characterXp"], 691);
        assert_eq!(v["wallet"][0]["balance"], 65_808_256i64);
        assert_eq!(v["character"]["pvpTrophies"], 773);
        assert_eq!(v["character"]["matchmakingPvpTrophies"], 847);
        // The stale pre-match meter (5) must be overwritten by the post-match 7.
        assert_eq!(v["character"]["pvpChestMeter"], 7);
        assert_eq!(v["character"]["pvpWinningStreak"], 1);
        assert_eq!(v["character"]["numberPvpMatchPlayed"], 138);
        assert_eq!(v["character"]["highestArenaReached"], 2);
        assert_eq!(v["character"]["highestLevelArenaReached"], 7);
        assert_eq!(v["currentRequestIndex"], 789416);
    }

    /// A promotion card. The shape is capture-derived, not authored: prod s168
    /// (flapdroid L5 crossing 50 trophies into arena 1 level 2, `chest_rarity` 3)
    /// carried exactly `{"chests":[{"id":"1","tier":3,"level":5}],"characterXp":0}`.
    #[test]
    fn results_json_carries_a_populated_reward_new_level_arena_on_promotion() {
        let reward = MatchEndReward {
            gold: 1170,
            character_xp: 42,
            pvp_trophies: 51,
            matchmaking_pvp_trophies: 51,
            pvp_chest_meter: 2,
            pvp_winning_streak: 1,
            number_pvp_match_played: 3,
            highest_arena_reached: 1,
            highest_level_arena_reached: 2,
            reward_new_level_arena: serde_json::json!({
                "chests": [ { "id": "1", "tier": 3, "level": 5 } ],
                "characterXp": 0,
            }),
            ..Default::default()
        };
        let out = results_json("aaaaaaaa-0000-0000-0000-000000000000", "{}", "{}", &reward, 1);
        let v: serde_json::Value = serde_json::from_str(&out).expect("ResultsJSON must parse");
        assert_eq!(v["rewardNewLevelArena"]["chests"][0]["tier"], 3);
        assert_eq!(v["rewardNewLevelArena"]["chests"][0]["level"], 5);
        assert_eq!(v["rewardNewLevelArena"]["characterXp"], 0);
        assert_eq!(v["character"]["highestLevelArenaReached"], 2);
    }

    /// op51 `ChangeCombatStatusEffect` byte shape (`docs/arena-status-resistance-spec.md`
    /// §5.3): carrier 0x36 on the Avatar, GMID 51, apply bool, status byte, duration,
    /// source-damage byte. A Poisoned(4.89s) apply with source 0 (an elemental condition).
    #[test]
    fn change_combat_status_effect_shape() {
        let got = change_combat_status_effect(125, true, StatusEffectType::Poisoned, 4.89, 0);
        assert_eq!(&got[0..2], &[0xBE, 0x36], "carrier 0x36");
        let nd = arena_proto::parse_netdata(&got[2..]);
        assert_eq!(nd.int(0), Some(125), "p0 actor obj");
        assert_eq!(nd.int(1), Some(56), "p1 Avatar");
        assert_eq!(nd.int(2), Some(1), "p2 Authority");
        assert_eq!(nd.int(3), Some(51), "p3 ChangeCombatStatusEffect gmid");
        assert_eq!(nd.props.get(&4), Some(&arena_proto::NetDataValue::Bool(true)), "p4 apply=true");
        assert_eq!(nd.int(5), Some(7), "p5 StatusEffectType Poisoned(7)");
        assert_eq!(nd.int(7), Some(0), "p7 sourceDamageType 0 (elemental condition)");

        // A Paralyzed(3.1s) apply rides the same shape (status byte 9).
        let par = change_combat_status_effect(125, true, StatusEffectType::Paralyzed, 3.1, 0);
        assert_eq!(arena_proto::parse_netdata(&par[2..]).int(5), Some(9), "Paralyzed = StatusEffectType 9");
    }

    /// Three REAL prod `PlayerChannelingStateChange` (53) frames, stored as their exact
    /// decrypted `user_data` hex. Each is `marker 0xBE ‖ carrier 0x36 ‖ NetData`.
    ///   * s127 #954966 — Lightning Bolt, propId-7 blob 7 B, propId 8 = 1.35024
    ///   * s127 #961429 — Fireball, blob 9 B, propId 8 = 1.21714
    ///   * s168 #1041484 — `cfee0b02…`, blob 7 B, propId 8 = 2.78354
    const OP53_S127_954966: &str = "be3609ff03707722d7a5350200003801351f000000f4216d251f000000ffffff3f04\
0704000000\
1c000483d4ac3f240037666331353830342d313633372d343061392d386463632d336561316562306637373864";
    const OP53_S127_961429: &str = "be3609ff03707722d7a53b020000380135340000006ffe6f3e34000000ffffff3f04\
09060000001c0001000439cb9b3f240064303761386433302d396131632d343962302d383636642d393761386161313533346366";
    const OP53_S168_1041484: &str = "be3609ff03707722d7a5140000003801351400000080feff3f14000000ffffff3f04\
07040000001c00044b253240240063666565306230322d366439312d346433342d383639632d613765353433323930363064";

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// **Byte-differential**: rebuild three real captured op53 frames from their decoded
    /// field values and assert byte-for-byte equality with the capture. This pins the
    /// carrier (`0x36`, NOT `0x35`), the propId set, the type nibbles, the ULong
    /// packed-stat pair, the constant propId 6 = 4, the u8-length `ByteArray` at propId
    /// 7, the float, and the trailing ability UUID string.
    #[test]
    fn player_channeling_state_change_matches_capture() {
        struct Case {
            hex: &'static str,
            obj: i32,
            caster: u64,
            opponent: u64,
            blob: &'static [u8],
            secs: f32,
            uuid: &'static str,
        }
        let cases = [
            Case {
                hex: OP53_S127_954966,
                obj: 565,
                caster: 0x256D_21F4_0000_001F,
                opponent: 0x3FFF_FFFF_0000_001F,
                blob: &[0x04, 0x00, 0x00, 0x00, 0x1C, 0x00, 0x04],
                secs: f32::from_bits(0x3FAC_D483),
                uuid: "7fc15804-1637-40a9-8dcc-3ea1eb0f778d",
            },
            Case {
                hex: OP53_S127_961429,
                obj: 571,
                caster: 0x3E6F_FE6F_0000_0034,
                opponent: 0x3FFF_FFFF_0000_0034,
                blob: &[0x06, 0x00, 0x00, 0x00, 0x1C, 0x00, 0x01, 0x00, 0x04],
                secs: f32::from_bits(0x3F9B_CB39),
                uuid: "d07a8d30-9a1c-49b0-866d-97a8aa1534cf",
            },
            Case {
                hex: OP53_S168_1041484,
                obj: 20,
                caster: 0x3FFF_FE80_0000_0014,
                opponent: 0x3FFF_FFFF_0000_0014,
                blob: &[0x04, 0x00, 0x00, 0x00, 0x1C, 0x00, 0x04],
                secs: f32::from_bits(0x4032_254B),
                uuid: "cfee0b02-6d91-4d34-869c-a7e54329060d",
            },
        ];
        for (i, c) in cases.iter().enumerate() {
            let want = unhex(c.hex);
            let got = player_channeling_state_change(
                c.obj,
                c.caster,
                c.opponent,
                c.secs,
                c.uuid,
                Some(c.blob),
            );
            assert_eq!(hex_of(&got), hex_of(&want), "op53 case {i} must be byte-identical to the capture");
            // The retail carrier is the UserMessage one; op53 is NOT a 0x35 net-object update.
            assert_eq!(got[1], MSGTYPE_USERMESSAGE, "op53 rides carrier 0x36");
            assert_eq!(user_message_gmid(&got), Some(53), "gmid 53 at propId 3");
            assert_eq!(retail_channel(&got), 0, "op53 rides ENet channel 0");
        }
    }

    /// Production emission omits the unmodelled propId-7 blob. NetData is a sparse
    /// property bag, so the frame stays well-formed and every other field is unchanged
    /// from the captured layout — asserted against the same s127 #954966 values.
    #[test]
    fn player_channeling_state_change_omits_the_unmodelled_blob() {
        let sparse = player_channeling_state_change(
            565,
            0x256D_21F4_0000_001F,
            0x3FFF_FFFF_0000_001F,
            0.9,
            "d07a8d30-9a1c-49b0-866d-97a8aa1534cf",
            None,
        );
        assert_eq!(&sparse[0..2], &[0xBE, 0x36]);
        let nd = arena_proto::parse_netdata(&sparse[2..]);
        assert!(nd.ok, "the sparse form is a well-formed NetData stream");
        assert_eq!(nd.int(0), Some(565));
        assert_eq!(nd.int(1), Some(56), "Avatar");
        assert_eq!(nd.int(2), Some(1), "Authority");
        assert_eq!(nd.int(3), Some(53));
        assert_eq!(nd.props.get(&4), Some(&arena_proto::NetDataValue::ULong(0x256D_21F4_0000_001F)));
        assert_eq!(nd.props.get(&5), Some(&arena_proto::NetDataValue::ULong(0x3FFF_FFFF_0000_001F)));
        assert_eq!(nd.int(6), Some(CHANNELING_STATE_ID as i64));
        assert!(!nd.props.contains_key(&7), "the unmodelled blob is omitted, not invented");
        assert_eq!(nd.props.get(&8), Some(&arena_proto::NetDataValue::Float(0.9)));
        assert_eq!(nd.string(9), Some("d07a8d30-9a1c-49b0-866d-97a8aa1534cf"));
    }

    /// **Byte-differential**: op64 `PerformConsumeConsumable` vs the real prod frame
    /// s127 #962751 (avatar 571 drinking "Potion of Light Healing").
    #[test]
    fn perform_consume_consumable_matches_capture() {
        let want = unhex(
            "be36041f70770a3b0200003801402400\
64383236656131322d653538332d343763312d613530662d346465363038323831373335",
        );
        let got = perform_consume_consumable(571, "d826ea12-e583-47c1-a50f-4de608281735");
        assert_eq!(hex_of(&got), hex_of(&want), "op64 must be byte-identical to the capture");
        assert_eq!(user_message_gmid(&got), Some(64));
        assert_eq!(retail_channel(&got), 0, "op64 rides ENet channel 0");
    }

    fn hex_of(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// op66 `DamageNegated` is a bare NetObjectInfo + GMID signal (no damage payload) on
    /// the defending Avatar — the Ward/Absorb full-negation path (§4.5). Routes on ch0.
    #[test]
    fn damage_negated_shape() {
        let got = damage_negated(125);
        assert_eq!(&got[0..2], &[0xBE, 0x36], "carrier 0x36");
        let nd = arena_proto::parse_netdata(&got[2..]);
        assert_eq!(nd.int(0), Some(125), "p0 defender obj");
        assert_eq!(nd.int(1), Some(56), "p1 Avatar");
        assert_eq!(nd.int(3), Some(66), "p3 DamageNegated gmid");
    }
}
