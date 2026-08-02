//! The **`PlayerStateChange` family** — the s2c frames that drive the client's
//! animation state machine.
//!
//! # Why this file exists
//!
//! Until now the arena server sent combat *results* and never the actor's *state*.
//! `op50 ReceiveDamage` moved the health bar, so damage looked right, while nothing
//! animated: no shield on a block, no weapon lifting on the opponent's swing, and
//! ability buttons that never learned when they were allowed back.
//!
//! Measured against retail session 503 (`arena_udp_frames`, s2c, grouped by
//! `game_message_id`), the messages we never sent were not a detail — they were most
//! of the conversation:
//!
//! | gmid | name | retail s503 | we sent |
//! |---|---|---|---|
//! | 39 | `PlayerStateChange` | 361 | none |
//! | 43 | `PlayerFollowThroughStateChange` | 325 | none |
//! | 52 | `PlayerAutoAttackStateChange` | 330 | none |
//! | 44 | `PlayerRecoveryStateChange` | 291 | none |
//! | 42 | `PlayerDrainingStateChange` | 50 | none |
//! | 59 | `InterruptAbility` | 32 | none |
//! | 41 | `PlayerBlockingStateChange` | 175 | only from unreachable code |
//!
//! That is 1,564 of roughly 4,000 s2c frames retail sent in that session.
//!
//! # The shared frame
//!
//! Every member of the family (39/41/42/43/44/45/52) is one inheritance chain, and
//! the property ids fall out of it base-first, in field-declaration order. From
//! `reference/il2cpp/dump.cs` in the `blades-capture` repo:
//!
//! ```text
//! BaseNetObjectMessage        dump.cs:429213   props 0,1,2   (PropId_Start = 3 @ 429216)
//!   NetObjectUserMessage      dump.cs:429329   prop  3       byte UserMsgId  (= GameMessageId)
//!     GameMessage             dump.cs:588391   — no fields
//!       PlayerStatsUpdateMessage    dump.cs:588413   props 4,5
//!         PlayerStateChangeMessage  dump.cs:590546   props 6,7,8
//!           <leaf>.Parameters                        props 9+
//! ```
//!
//! Every wire type below is **capture-pinned**, decoded from 593 `decrypt_status='ok'`
//! frames across prod sessions 503 / 615 / 616. Each gmid has exactly one prop-set
//! variant — the `maxPropId + bitmap + type-nibble` prefix is byte-identical within a
//! gmid, with no exceptions in the corpus.
//!
//! | prop | field | wire type | dump.cs |
//! |---|---|---|---|
//! | 0 | `NetObjectInfo.NetObjectId` | Int | 428736 |
//! | 1 | `NetObjectInfo.NetObjectType` | Byte — const **56** (Avatar) | 428738 |
//! | 2 | `NetObjectInfo.NetRole` | Byte — const **1** (Authority) | 428740 |
//! | 3 | `NetObjectUserMessage.UserMsgId` | Byte = GameMessageId | 429332 |
//! | 4 | `_pvpThisActorStats` | **ULong** — the actor's own packed stats | 588416 |
//! | 5 | `_pvpOtherActorStats` | **ULong** — the opponent's | 588417 |
//! | 6 | `_stateId` | Byte — `ActorStateType.StateId` | 590549 |
//! | 7 | `_stateHistory` | **ByteArray** — see [`super::state::Fighter::packed_state_history`] | 590550 |
//! | 8 | `_timeInPreviousState` | Float | 590551 |
//! | 9+ | the leaf's own `Parameters` | per message | — |
//!
//! **propId 9 is not uniformly typed.** Retail writes it as a `Byte` for gmid
//! 42/43/45/52 and as a 4-byte `Int` for gmid 41 and 44. It is a quirk, not a
//! pattern, and it is reproduced here per-gmid because the type nibble changes the
//! byte layout of everything after it.
//!
//! The derivation was calibrated against the two members we already ship that are
//! pinned to real captured bytes — gmid 45 (13,060 frames) and gmid 41 (400 frames)
//! — and it reproduces both prop-for-prop.
//!
//! # What actually drives the animation
//!
//! `PvpAvatar::OnStateChangeMessage` (dump.cs:583534) → `ApplyStateChange`
//! (dump.cs:583535), whose `forceStateChange` argument defaults to true — the server
//! is authoritative and the client will not veto a transition. It reads prop 6 (the
//! state id), the leaf `ActiveSide` at prop 9, and for gmid 52 the `Vector2` at prop
//! 10. Props 7 and 8 feed reconciliation, not the clip.
//!
//! An out-of-range prop 6 is the one silent failure mode:
//! `ActorStateType.FindStateTypeByID` (dump.cs:340164) returns null for anything
//! outside 0..=28 and the state change evaporates with no error. Every id sent from
//! here comes from [`ActorStateType`], transcribed from the dump.
//!
//! # Both viewers, always
//!
//! The opponent's avatar resolves the same state id through a different factory —
//! `PvpOpponentAutoAttackState` (dump.cs:597457), `PvpOpponentFollowThroughState`
//! (597576), `PvpOpponentRecoveryState` (597615), `PvpOpponentBlockingState` (597502)
//! — all registered in the same `StateId` space by `RegisterStates()`
//! (dump.cs:337136). One frame, sent unchanged to both players, animates the actor on
//! its own screen and the opponent on the other.
//!
//! The captures settle it independently: a single client's capture contains
//! state-change frames for **both** avatar net-object ids (s615 carries 430 and 431,
//! s616 carries 561 and 562, s503 one pair per round), and props 4/5 are "own" and
//! "other" *relative to prop 0*, which only makes sense if a client receives frames
//! authored for the opponent's avatar too. Sending to only the acting player is
//! exactly the bug that made enemy swings invisible.

use arena_proto::{GameMessageId, NetDataValue, NetDataWriter};

use super::messages::{frame, MSGTYPE_USERMESSAGE};
use super::state::{ActiveSide, ActorStateType, NetObjectType, NetRole};

/// Everything a `PlayerStateChange`-family frame needs that is the same for all of
/// them: who it is about, both fighters' packed stats, and the state-history ring.
///
/// Bundled because props 0..7 are shared by seven message ids and threading five
/// arguments through each builder invites a transposition at one call site.
#[derive(Debug, Clone)]
pub struct StateFrame<'a> {
    /// The **Avatar** net object id (not the Player or Ability id).
    pub actor_net_object_id: i32,
    /// The actor's own packed stats word, `Fighter::packed_stats()`.
    pub own_packed_stats: u64,
    /// The opponent's packed stats word.
    pub opponent_packed_stats: u64,
    /// `Fighter::packed_state_history()` — propId 7, already in wire layout.
    pub state_history: &'a [u8],
}

impl StateFrame<'_> {
    /// Write props 0..8: the whole shared prefix.
    fn prefix(
        &self,
        w: &mut NetDataWriter,
        gmid: GameMessageId,
        state: ActorStateType,
        time_in_previous_state: f32,
    ) {
        w.int(0, self.actor_net_object_id)
            .byte(1, NetObjectType::Avatar as u8) // 56
            .byte(2, NetRole::Authority as u8) // 1
            .byte(3, gmid as u8)
            // ULong, not Long: retail's type nibble is 2 in all 593 decoded frames.
            .ulong(4, self.own_packed_stats)
            .ulong(5, self.opponent_packed_stats)
            .byte(6, state as u8)
            .put(7, NetDataValue::ByteArray(self.state_history.to_vec()))
            .float(8, time_in_previous_state);
    }
}

/// An `ActiveSide` that is safe to put on the wire.
///
/// `PlayerBlockingState.Parameters.Validate` (dump.cs:597112) and
/// `PlayerRecoveryState.Parameters.Validate` (dump.cs:597423) are the two in the
/// family that are NOT empty virtual stubs, so a nonsense side is the plausible way
/// to get a frame rejected. Captures only ever show 1/2/3 across the whole family,
/// never 0 — so `None` is folded to `Middle` rather than sent.
fn wire_side(side: ActiveSide) -> u8 {
    match side {
        ActiveSide::None => ActiveSide::Middle as u8,
        s => s as u8,
    }
}

/// `_timeInPreviousState` for gmid 43. Capture-pinned: 0.050002…0.0667 across the
/// corpus, i.e. three frames at 60 Hz, and the measured 52→43 gap is 49–65 ms.
pub const FOLLOW_THROUGH_TIME_IN_PREV: f32 = 0.050_002;
/// `_timeInPreviousState` for gmid 44 — exactly one 60 Hz frame, and the measured
/// 43→44 gap is 16–21 ms.
pub const RECOVERY_TIME_IN_PREV: f32 = 0.016_667_6;
/// `_timeInPreviousState` for gmid 42. Capture-pinned: the median of 34 decoded
/// frames is 0.350022, tightly clustered.
pub const DRAINING_TIME_IN_PREV: f32 = 0.350_022;

/// gmid 39 `PlayerStateChange` — the family's **generic** member: the chain stops at
/// `PlayerStateChangeMessage` (dump.cs:590546) with no leaf and no `Parameters`, so
/// the frame is props 0..8 and nothing else. Confirmed on the wire: every decoded
/// gmid 39 frame carries the prop set {0..8} and no propId 9.
///
/// It is how every state without a dedicated message id is announced. Decoded prop-6
/// values in retail: `0` Idle (75 frames), `5` Staggered (8), `28` Emote (7), `27`
/// OpponentVictory (1) — exactly the states with no message of their own.
///
/// **This is also how a block ENDS.** There is no "shield down" variant of gmid 41:
/// all 248 decoded 41 frames carry prop6 = Blocking, and consecutive 41s for one
/// avatar show `…Idle, Blocking` appended to the history each time, i.e. a fresh
/// *entry* into Blocking. The guard comes down with a gmid 39 carrying prop6 = 0.
pub fn player_state_change(
    ctx: &StateFrame<'_>,
    state: ActorStateType,
    time_in_previous_state: f32,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    ctx.prefix(
        &mut w,
        GameMessageId::PlayerStateChange,
        state,
        time_in_previous_state,
    );
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// gmid 41 `PlayerBlockingStateChange` — **the frame that raises the shield.**
///
/// `PlayerBlockingStateChangeMessage` (dump.cs:590637), tail
/// `PlayerBlockingState.Parameters` (dump.cs:597104): `ActiveSide` at prop 9 (597107)
/// and `bool OptimalBlockAllowed` at prop 10 (597108).
///
/// Capture-pinned over 248 decoded frames:
/// * prop 6 is **1 (Blocking)** in 248/248 — there is no shield-down variant;
/// * prop 9 is **`Int`-typed** (4 bytes, unlike 42/43/45/52's Byte) and is **always
///   1 (Middle)**. A block is centred; it has no left/right;
/// * prop 10 is a Bool, `true` in 231 of 248.
///
/// **prop 10 is a documented gap.** The dump names it `OptimalBlockAllowed`, but no
/// decoded correlation explains the 17 `false` frames: they are not block-end (prop 6
/// is still Blocking), not tied to an actor, a time, a sequence, or a history shape,
/// and consecutive `false`s occur. `optimal_block_allowed` therefore defaults to the
/// 94 % value at the call site rather than being derived from a guessed meaning.
///
/// This replaces [`super::messages::player_blocking_state_change`], which took a
/// `blocking: bool` for prop 10 and hardcoded props 4/5/7/8 to one round's captured
/// constants.
pub fn player_blocking_state_change(
    ctx: &StateFrame<'_>,
    time_in_previous_state: f32,
    optimal_block_allowed: bool,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    ctx.prefix(
        &mut w,
        GameMessageId::PlayerBlockingStateChange,
        ActorStateType::Blocking,
        time_in_previous_state,
    );
    // Int, not Byte — retail's nibble for prop 9 here is 0, and always the value 1.
    w.int(9, ActiveSide::Middle as i32).bool(10, optimal_block_allowed);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// gmid 52 `PlayerAutoAttackStateChange` — **the missing enemy swing.**
///
/// `PlayerAutoAttackStateChangeMessage` (dump.cs:590736), tail
/// `PlayerAutoAttackState.Parameters` (dump.cs:597041): `ActiveSide InitialActiveSide`
/// at prop 9 (597044, offset 0x10) and `Vector2 Direction` at prop 10 (597045, 0x14).
///
/// Capture-pinned: prop 6 = **19** in 25/25, prop 9 is a **Byte** and only ever 2 or 3
/// (Left/Right — an auto-attack always has a swipe direction), prop 10 is a genuine
/// `Vector2` type tag carrying two f32 LE.
///
/// `direction` is the normalised swipe vector; the four non-zero captured values are
/// unit vectors to within 1e-4. It is **`(0.0, 0.0)` in 21 of 25 retail frames**, so
/// zero is the well-supported default when the client streamed no pointer geometry —
/// not a placeholder we invented.
pub fn player_auto_attack_state_change(
    ctx: &StateFrame<'_>,
    side: ActiveSide,
    direction: (f32, f32),
    time_in_previous_state: f32,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    ctx.prefix(
        &mut w,
        GameMessageId::PlayerAutoAttackStateChange,
        ActorStateType::PlayerAutoAttack,
        time_in_previous_state,
    );
    let mut v = [0u8; 8];
    v[..4].copy_from_slice(&direction.0.to_le_bytes());
    v[4..].copy_from_slice(&direction.1.to_le_bytes());
    w.byte(9, wire_side(side)).put(10, NetDataValue::Vector2(v));
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// gmid 43 `PlayerFollowThroughStateChange` — the second beat of a swing.
///
/// `PlayerFollowThroughStateChangeMessage` (dump.cs:590769), tail
/// `PlayerFollowThroughState.Parameters` (dump.cs:597332): a single `ActiveSide
/// InitialActiveSide` at prop 9 (597335). State id `PlayerFollowThrough = 17`
/// (dump.cs:340192), confirmed 21/21 on the wire. prop 9 is a **Byte**, 2 or 3.
pub fn player_follow_through_state_change(
    ctx: &StateFrame<'_>,
    side: ActiveSide,
    time_in_previous_state: f32,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    ctx.prefix(
        &mut w,
        GameMessageId::PlayerFollowThroughStateChange,
        ActorStateType::PlayerFollowThrough,
        time_in_previous_state,
    );
    w.byte(9, wire_side(side));
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// gmid 44 `PlayerRecoveryStateChange` — **the ability-button gate.**
///
/// `PlayerRecoveryStateChangeMessage` (dump.cs:590802), tail
/// `PlayerRecoveryState.Parameters` (dump.cs:597415): a single `ActiveSide
/// InitialSide` at prop 9 (597418 — the field name differs from the other leaves'
/// `InitialActiveSide`). State id `PlayerRecovery = 16` (dump.cs:340191), confirmed
/// 22/22.
///
/// prop 9 here is an **`Int`**, not a Byte — the same quirk as gmid 41, and the
/// reason this builder cannot just share a tail helper with 43/52.
///
/// `PlayerRecoveryState` is what greys the buttons out and back:
/// `RecoveryProgress` (597375), `CanBeginCharging` (597400),
/// `CanBeginRegularAttack` (597402), `CanBeginCombo` (597404), `CanBeginBlock`
/// (597406). The message carries **no duration** — the recovery length is
/// client-side, driven purely by entry into the state. So the only thing the server
/// has to get right is sending the transition at all, which is why never sending 44
/// is the likely cause of the ability-cooldown complaints.
pub fn player_recovery_state_change(
    ctx: &StateFrame<'_>,
    side: ActiveSide,
    time_in_previous_state: f32,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    ctx.prefix(
        &mut w,
        GameMessageId::PlayerRecoveryStateChange,
        ActorStateType::PlayerRecovery,
        time_in_previous_state,
    );
    w.int(9, wire_side(side) as i32);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// gmid 42 `PlayerDrainingStateChange` — a life/magicka leech in progress.
///
/// `PlayerDrainingStateChangeMessage` (dump.cs:590670), tail
/// `PlayerDrainingState.Parameters` (dump.cs:597281): a single `ActiveSide
/// InitialActiveSide` at prop 9 (597284), a **Byte**, 2 or 3. State id
/// `PlayerDraining = 14` (dump.cs:340189), confirmed 34/34.
///
/// `PlayerDrainingState` is the only one of the seven that overrides
/// `IsReplicated()` (dump.cs:597253, base at 340347). The body is not in the dump; if
/// it returns false the client suppresses its *own* replication of the state, which
/// does not affect an authoritative s2c relay.
pub fn player_draining_state_change(
    ctx: &StateFrame<'_>,
    side: ActiveSide,
    time_in_previous_state: f32,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    ctx.prefix(
        &mut w,
        GameMessageId::PlayerDrainingStateChange,
        ActorStateType::PlayerDraining,
        time_in_previous_state,
    );
    w.byte(9, wire_side(side));
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// gmid 59 `InterruptAbility` — cancel a cast that is already running.
///
/// **Not** a state change: `InterruptAbilityMessage : GameMessage` (dump.cs:588879)
/// extends `GameMessage` directly, so there are no packed stats and no state id, and
/// the payload starts at prop 4. Confirmed on the wire — prop set {0..5}:
///
/// | prop | field | type | dump.cs |
/// |---|---|---|---|
/// | 4 | `string _abilityId` | String, u16 length, always 36 | 588882 |
/// | 5 | `bool _selfInterrupt` | Bool | 588883 |
///
/// The client resolves it through `PvpAvatar::OnAbilityMessage` (dump.cs:583545) →
/// `GetActiveAbility(string abilityId)` (583541); the server-side counterpart
/// `RequestInterruptAbility(ActiveAbility, bool)` (dump.cs:338462, overridden at
/// 343718/599507) confirms the two-argument shape end to end.
///
/// **`ability_id` must be the exact 36-char UUID the client sent in gmid 37
/// `RequestExecuteAbility`.** If `GetActiveAbility` cannot resolve it against the
/// target's loadout the interrupt is dropped silently — no error, no visible effect.
///
/// On `self_interrupt`: the dump gives the name, and the captures give a behavioural
/// hint — a gmid 59 with prop 5 = `true` is repeatedly followed within a millisecond
/// by a gmid 41 for the same avatar (raising a guard cancels the cast), while
/// `false` frames never are. That is suggestive, not proof; treat the flag's meaning
/// as PLAUSIBLE.
pub fn interrupt_ability(
    actor_net_object_id: i32,
    ability_id: &str,
    self_interrupt: bool,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, actor_net_object_id)
        .byte(1, NetObjectType::Avatar as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::InterruptAbility as u8)
        .string(4, ability_id)
        .bool(5, self_interrupt);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// gmid 75 `PlayerDestroyedStatUpdate` — the greyed-out tail on a bar.
///
/// `PlayerDestroyedStatUpdateMessage : GameMessage, IPooledObject` (dump.cs:588478) —
/// also outside the state-change family. Confirmed on the wire, prop set {0..5}, the
/// smallest frame in the family at 29 bytes:
///
/// | prop | field | wire type | dump.cs |
/// |---|---|---|---|
/// | 4 | `ActorStats.CoreStats _statType` | **Byte** — Health=0, Stamina=1, Magicka=2 | 588481 |
/// | 5 | `float _destroyedPortion` | Float | 588482 |
///
/// Distinct from `PlayerStatsUpdate` (65), which moves the *current* value: this says
/// a portion of the **maximum** is gone. `messages::retail_channel` already routes 75
/// to ENet channel 1 alongside 65.
///
/// **`destroyed_portion` is a running total in absolute stat points, not a 0..1
/// fraction** — capture-pinned, and it contradicts the field name. It rises
/// monotonically per `(avatar, statType)`: one avatar's stamina track runs 42, 84,
/// 126, 168, 210, 252, 294 in steps of 42; another's runs 126.64, 158.30, 189.96,
/// 221.62, 253.28 in steps of 31.66. Pass the cumulative amount destroyed so far,
/// never the per-hit delta.
///
/// The `CoreStats` mapping (dump.cs:599917) is corroborated per-fighter: the fighter
/// whose stamina bits drain to zero is the one receiving `statType = 1`.
pub fn player_destroyed_stat_update(
    actor_net_object_id: i32,
    stat: CoreStat,
    destroyed_total: f32,
) -> Vec<u8> {
    let mut w = NetDataWriter::new();
    w.int(0, actor_net_object_id)
        .byte(1, NetObjectType::Avatar as u8)
        .byte(2, NetRole::Authority as u8)
        .byte(3, GameMessageId::PlayerDestroyedStatUpdate as u8)
        .byte(4, stat as u8)
        .float(5, destroyed_total);
    frame(MSGTYPE_USERMESSAGE, w.finish())
}

/// `ActorStats.CoreStats` (dump.cs:599917) — which bar a
/// [`player_destroyed_stat_update`] refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CoreStat {
    Health = 0,
    Stamina = 1,
    Magicka = 2,
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena_proto::parse_netdata;

    const HISTORY: &[u8] = &[3, 0, 0, 0x00, 0x1c, 0x01];

    fn ctx() -> StateFrame<'static> {
        StateFrame {
            actor_net_object_id: 398,
            own_packed_stats: 0x0000_002A_A285_EB35,
            opponent_packed_stats: 0x0000_002A_59AA_FC3F,
            state_history: HISTORY,
        }
    }

    /// Decode a built frame back into its properties, asserting the 0xBE marker and
    /// the UserMessage carrier on the way.
    fn props(frame: &[u8]) -> arena_proto::NetDataParse {
        assert_eq!(frame[0], 0xBE, "s2c marker");
        assert_eq!(frame[1], MSGTYPE_USERMESSAGE, "carrier 0x36");
        let nd = parse_netdata(&frame[2..]);
        assert!(nd.ok, "frame must parse cleanly");
        assert_eq!(nd.consumed, frame.len() - 2, "no trailing bytes");
        nd
    }

    /// Every family member carries the same 0..8 prefix, addressed to the Avatar net
    /// object with Authority role — that is what `PvpAvatar` matches on.
    #[test]
    fn family_shares_the_capture_pinned_prefix() {
        let c = ctx();
        let cases: Vec<(i64, Vec<u8>)> = vec![
            (39, player_state_change(&c, ActorStateType::Idle, 0.25)),
            (41, player_blocking_state_change(&c, 0.30, true)),
            (
                52,
                player_auto_attack_state_change(&c, ActiveSide::Right, (0.0, 0.0), 0.25),
            ),
            (
                43,
                player_follow_through_state_change(&c, ActiveSide::Right, FOLLOW_THROUGH_TIME_IN_PREV),
            ),
            (
                44,
                player_recovery_state_change(&c, ActiveSide::Right, RECOVERY_TIME_IN_PREV),
            ),
            (
                42,
                player_draining_state_change(&c, ActiveSide::Left, DRAINING_TIME_IN_PREV),
            ),
        ];
        for (gmid, f) in cases {
            let nd = props(&f);
            assert_eq!(nd.int(0), Some(398), "gmid {gmid}: prop0 avatar net object id");
            assert_eq!(nd.int(1), Some(56), "gmid {gmid}: prop1 NetObjectType::Avatar");
            assert_eq!(nd.int(2), Some(1), "gmid {gmid}: prop2 NetRole::Authority");
            assert_eq!(nd.int(3), Some(gmid), "gmid {gmid}: prop3 GameMessageId");
            // ULong, not Long — retail's type nibble is 2 in all 593 decoded frames.
            assert!(
                matches!(nd.get(4), Some(NetDataValue::ULong(_))),
                "gmid {gmid}: prop4 must be ULong, got {:?}",
                nd.get(4)
            );
            assert!(
                matches!(nd.get(5), Some(NetDataValue::ULong(_))),
                "gmid {gmid}: prop5 must be ULong"
            );
            assert!(
                matches!(nd.get(6), Some(NetDataValue::Byte(_))),
                "gmid {gmid}: prop6 stateId must be a Byte"
            );
            // ByteArray, NOT String: `PackStateHistory` can only feed SetProperty(byte,
            // byte[], int). The pre-existing gmid 41/45 builders get this wrong.
            assert_eq!(
                nd.get(7),
                Some(&NetDataValue::ByteArray(HISTORY.to_vec())),
                "gmid {gmid}: prop7 must be the ByteArray history"
            );
            assert!(
                matches!(nd.get(8), Some(NetDataValue::Float(_))),
                "gmid {gmid}: prop8 must be a Float"
            );
        }
    }

    /// prop 6 must be the dump's `ActorStateType.StateId`, per message. A wrong value
    /// here is the silent failure: `FindStateTypeByID` returns null outside 0..=28 and
    /// the client drops the state change without an error.
    #[test]
    fn state_ids_match_the_dump_and_the_captures() {
        let c = ctx();
        let cases = [
            (
                19i64,
                player_auto_attack_state_change(&c, ActiveSide::Right, (0.0, 0.0), 0.0),
            ),
            (17, player_follow_through_state_change(&c, ActiveSide::Right, 0.0)),
            (16, player_recovery_state_change(&c, ActiveSide::Right, 0.0)),
            (14, player_draining_state_change(&c, ActiveSide::Right, 0.0)),
            (1, player_blocking_state_change(&c, 0.0, true)),
        ];
        for (want, f) in cases {
            assert_eq!(props(&f).int(6), Some(want), "prop6 stateId");
        }
        // The generic member carries whatever state it is announcing. These four are
        // the values retail was decoded sending on gmid 39.
        for state in [
            ActorStateType::Idle,
            ActorStateType::Staggered,
            ActorStateType::OpponentVictory,
            ActorStateType::Emote,
        ] {
            let f = player_state_change(&c, state, 0.0);
            assert_eq!(props(&f).int(6), Some(state as i64), "gmid 39 prop6 = {state:?}");
        }
    }

    /// gmid 39 has no leaf `Parameters`, so it must stop at prop 8. Emitting a
    /// spurious prop 9 would make the client read a side retail never sent.
    #[test]
    fn generic_state_change_has_no_leaf_parameters() {
        let nd = props(&player_state_change(&ctx(), ActorStateType::Idle, 1.5));
        assert_eq!(nd.get(8), Some(&NetDataValue::Float(1.5)));
        assert!(!nd.props.contains_key(&9), "gmid 39 must NOT carry an ActiveSide");
    }

    /// propId 9's WIDTH is not uniform across the family: Int for 41 and 44, Byte for
    /// 42/43/52. Getting it wrong desynchronises every byte after it.
    #[test]
    fn prop9_width_matches_retail_per_gmid() {
        let c = ctx();
        let as_int = [
            ("41", player_blocking_state_change(&c, 0.0, true)),
            ("44", player_recovery_state_change(&c, ActiveSide::Right, 0.0)),
        ];
        for (gmid, f) in as_int {
            assert!(
                matches!(props(&f).get(9), Some(NetDataValue::Int(_))),
                "gmid {gmid}: prop9 must be Int"
            );
        }
        let as_byte = [
            ("42", player_draining_state_change(&c, ActiveSide::Right, 0.0)),
            ("43", player_follow_through_state_change(&c, ActiveSide::Right, 0.0)),
            (
                "52",
                player_auto_attack_state_change(&c, ActiveSide::Right, (0.0, 0.0), 0.0),
            ),
        ];
        for (gmid, f) in as_byte {
            assert!(
                matches!(props(&f).get(9), Some(NetDataValue::Byte(_))),
                "gmid {gmid}: prop9 must be Byte"
            );
        }
    }

    /// A block is centred: prop 9 is 1 (Middle) in all 248 decoded retail frames,
    /// regardless of where the player's finger was.
    #[test]
    fn block_is_always_middle() {
        let nd = props(&player_blocking_state_change(&ctx(), 0.3, true));
        assert_eq!(nd.int(9), Some(ActiveSide::Middle as i64));
        assert_eq!(nd.get(10), Some(&NetDataValue::Bool(true)));
    }

    /// `ActiveSide::None` (0) is never on the wire — captures only ever show 1/2/3,
    /// and `PlayerRecoveryState.Parameters.Validate` is one of the two non-stub
    /// validators that could reject it.
    #[test]
    fn active_side_none_is_folded_to_middle() {
        let c = ctx();
        let f = player_recovery_state_change(&c, ActiveSide::None, 0.0);
        assert_eq!(props(&f).int(9), Some(ActiveSide::Middle as i64));
        for side in [ActiveSide::Middle, ActiveSide::Left, ActiveSide::Right] {
            let f = player_recovery_state_change(&c, side, 0.0);
            assert_eq!(props(&f).int(9), Some(side as i64), "{side:?} passes through");
        }
    }

    /// gmid 52's prop 10 is the swipe direction, and it is what picks the swing clip.
    /// Two f32 LE inside a `Vector2`-tagged property.
    #[test]
    fn auto_attack_carries_the_swipe_direction() {
        let nd = props(&player_auto_attack_state_change(
            &ctx(),
            ActiveSide::Left,
            (0.932_77, 0.360_40),
            0.0,
        ));
        match nd.get(10) {
            Some(NetDataValue::Vector2(v)) => {
                assert_eq!(f32::from_le_bytes(v[..4].try_into().unwrap()), 0.932_77);
                assert_eq!(f32::from_le_bytes(v[4..].try_into().unwrap()), 0.360_40);
            }
            other => panic!("prop10 must be a Vector2, got {other:?}"),
        }
    }

    /// gmid 59 is outside the family: no packed stats, no state id, payload at 4/5.
    #[test]
    fn interrupt_ability_is_ability_id_and_flag() {
        let uuid = "4be1d681-c35d-4540-b255-c2910ac80664";
        let nd = props(&interrupt_ability(91, uuid, true));
        assert_eq!(nd.int(3), Some(59));
        assert_eq!(nd.string(4), Some(uuid));
        assert_eq!(nd.get(5), Some(&NetDataValue::Bool(true)));
        assert!(!nd.props.contains_key(&6), "gmid 59 carries no stateId");
    }

    /// gmid 75 selects a bar with a **Byte** and carries a cumulative absolute
    /// amount, not a 0..1 fraction.
    #[test]
    fn destroyed_stat_update_selects_a_bar() {
        let nd = props(&player_destroyed_stat_update(281, CoreStat::Health, 386.64));
        assert_eq!(nd.int(3), Some(75));
        assert!(
            matches!(nd.get(4), Some(NetDataValue::Byte(0))),
            "prop4 CoreStats::Health as a Byte"
        );
        assert_eq!(nd.get(5), Some(&NetDataValue::Float(386.64)));
        assert!(!nd.props.contains_key(&6), "gmid 75 stops at prop 5");
    }
}
