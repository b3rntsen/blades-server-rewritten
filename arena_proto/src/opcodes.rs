//! `GameMessageId` — the opcode byte at `user_data[1]` in arena UDP messages.
//!
//! Ported verbatim from `emulator/internal/proto/opcodes.go`, itself codegen'd
//! from `reference/il2cpp/arena-opcodes.json`. The `GAME_MESSAGE_IDS` set below
//! must stay identical to the `GAMEMESSAGE_IDS` frozenset in
//! `scripts/arena-decrypt.py` — it's the semantic gate the decrypt worker uses
//! to populate the `opcode` column, and the replay harness asserts parity
//! against that column.
//!
//! (Other il2cpp enums — `GameAction`, `StatusEffectType`, `CombatLog*`,
//! `ActionEvent` — are deferred to the combat-decode milestone; they aren't
//! needed for crypto/ENet/opcode parity.)

/// The set of defined `GameMessageId` values (matches the Python
/// `GAMEMESSAGE_IDS` frozenset exactly). Note the gaps: 25, 62, 67, 68, 69 are
/// not assigned in this APK's enum.
pub const GAME_MESSAGE_IDS: &[u8] = &[
    20, 21, 22, 23, 24, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43,
    44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 63, 64, 65, 66, 70,
    71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83,
];

/// True iff `b` is a defined `GameMessageId` — the "this is a real game message"
/// gate used when recording a frame's opcode.
#[inline]
pub fn is_game_message_id(b: u8) -> bool {
    GAME_MESSAGE_IDS.contains(&b)
}

/// Opcode byte at `user_data[1]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum GameMessageId {
    PlayerInfo = 20,
    PlayerWelcome = 21,
    PlayerSpawnAvatar = 22,
    GamePing = 23,
    PlayerCommand = 24,
    FairnessLatency = 26,
    PlayerHealth = 27,
    ConcedeMatch = 28,
    PlayerDeadStateChange = 29,
    SetServerCheat = 30,
    PlayerInputCancelCharge = 31,
    SpawnTargetDummy = 32,
    ServerErrorOrAssert = 33,
    DestroyTargetDummy = 34,
    OpponentLoadout = 35,
    PlayerLoadoutReady = 36,
    RequestExecuteAbility = 37,
    PerformExecuteAbility = 38,
    PlayerStateChange = 39,
    PlayerAttackStateChange = 40,
    PlayerBlockingStateChange = 41,
    PlayerDrainingStateChange = 42,
    PlayerFollowThroughStateChange = 43,
    PlayerRecoveryStateChange = 44,
    PlayerChargingStateChange = 45,
    PlayerCombatInputActivate = 46,
    PlayerCombatInputPosition = 47,
    MatchPostRoundInfoMsg = 48,
    MatchEndMatchMsg = 49,
    ReceiveDamage = 50,
    ChangeCombatStatusEffect = 51,
    PlayerAutoAttackStateChange = 52,
    PlayerChannelingStateChange = 53,
    CombatSwipeInfo = 54,
    CombatScreenInfo = 55,
    EquipAbilitiesAndConsumables = 56,
    SkipCurrentState = 57,
    PlayerManeuverStateChange = 58,
    InterruptAbility = 59,
    StopAbility = 60,
    LoadoutClientBackendSynchronized = 61,
    RequestConsumeConsumable = 63,
    PerformConsumeConsumable = 64,
    PlayerStatsUpdate = 65,
    DamageNegated = 66,
    ServerGcCount = 70,
    RecordUrls = 71,
    PlayEmote = 72,
    PlayerEmoteStateChange = 73,
    SyncStartRecording = 74,
    PlayerDestroyedStatUpdate = 75,
    ClientAppPaused = 76,
    ServerShutdown = 77,
    PlayerPlayVFX = 78,
    MatchStateChangeRequest = 79,
    MatchStateChangeAck = 80,
    RestoreActor = 81,
    CombatLog = 82,
    ModifyAbilityCooldowns = 83,
}

impl GameMessageId {
    /// Parse a raw opcode byte. Returns `None` for undefined values.
    pub fn from_u8(b: u8) -> Option<Self> {
        use GameMessageId::*;
        Some(match b {
            20 => PlayerInfo,
            21 => PlayerWelcome,
            22 => PlayerSpawnAvatar,
            23 => GamePing,
            24 => PlayerCommand,
            26 => FairnessLatency,
            27 => PlayerHealth,
            28 => ConcedeMatch,
            29 => PlayerDeadStateChange,
            30 => SetServerCheat,
            31 => PlayerInputCancelCharge,
            32 => SpawnTargetDummy,
            33 => ServerErrorOrAssert,
            34 => DestroyTargetDummy,
            35 => OpponentLoadout,
            36 => PlayerLoadoutReady,
            37 => RequestExecuteAbility,
            38 => PerformExecuteAbility,
            39 => PlayerStateChange,
            40 => PlayerAttackStateChange,
            41 => PlayerBlockingStateChange,
            42 => PlayerDrainingStateChange,
            43 => PlayerFollowThroughStateChange,
            44 => PlayerRecoveryStateChange,
            45 => PlayerChargingStateChange,
            46 => PlayerCombatInputActivate,
            47 => PlayerCombatInputPosition,
            48 => MatchPostRoundInfoMsg,
            49 => MatchEndMatchMsg,
            50 => ReceiveDamage,
            51 => ChangeCombatStatusEffect,
            52 => PlayerAutoAttackStateChange,
            53 => PlayerChannelingStateChange,
            54 => CombatSwipeInfo,
            55 => CombatScreenInfo,
            56 => EquipAbilitiesAndConsumables,
            57 => SkipCurrentState,
            58 => PlayerManeuverStateChange,
            59 => InterruptAbility,
            60 => StopAbility,
            61 => LoadoutClientBackendSynchronized,
            63 => RequestConsumeConsumable,
            64 => PerformConsumeConsumable,
            65 => PlayerStatsUpdate,
            66 => DamageNegated,
            70 => ServerGcCount,
            71 => RecordUrls,
            72 => PlayEmote,
            73 => PlayerEmoteStateChange,
            74 => SyncStartRecording,
            75 => PlayerDestroyedStatUpdate,
            76 => ClientAppPaused,
            77 => ServerShutdown,
            78 => PlayerPlayVFX,
            79 => MatchStateChangeRequest,
            80 => MatchStateChangeAck,
            81 => RestoreActor,
            82 => CombatLog,
            83 => ModifyAbilityCooldowns,
            _ => return None,
        })
    }

    /// The raw opcode byte.
    #[inline]
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

impl std::fmt::Display for GameMessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            GameMessageId::PlayerInfo => "PlayerInfo",
            GameMessageId::PlayerWelcome => "PlayerWelcome",
            GameMessageId::PlayerSpawnAvatar => "PlayerSpawnAvatar",
            GameMessageId::GamePing => "GamePing",
            GameMessageId::PlayerCommand => "PlayerCommand",
            GameMessageId::FairnessLatency => "FairnessLatency",
            GameMessageId::PlayerHealth => "PlayerHealth",
            GameMessageId::ConcedeMatch => "ConcedeMatch",
            GameMessageId::PlayerDeadStateChange => "PlayerDeadStateChange",
            GameMessageId::SetServerCheat => "SetServerCheat",
            GameMessageId::PlayerInputCancelCharge => "PlayerInputCancelCharge",
            GameMessageId::SpawnTargetDummy => "SpawnTargetDummy",
            GameMessageId::ServerErrorOrAssert => "ServerErrorOrAssert",
            GameMessageId::DestroyTargetDummy => "DestroyTargetDummy",
            GameMessageId::OpponentLoadout => "OpponentLoadout",
            GameMessageId::PlayerLoadoutReady => "PlayerLoadoutReady",
            GameMessageId::RequestExecuteAbility => "RequestExecuteAbility",
            GameMessageId::PerformExecuteAbility => "PerformExecuteAbility",
            GameMessageId::PlayerStateChange => "PlayerStateChange",
            GameMessageId::PlayerAttackStateChange => "PlayerAttackStateChange",
            GameMessageId::PlayerBlockingStateChange => "PlayerBlockingStateChange",
            GameMessageId::PlayerDrainingStateChange => "PlayerDrainingStateChange",
            GameMessageId::PlayerFollowThroughStateChange => "PlayerFollowThroughStateChange",
            GameMessageId::PlayerRecoveryStateChange => "PlayerRecoveryStateChange",
            GameMessageId::PlayerChargingStateChange => "PlayerChargingStateChange",
            GameMessageId::PlayerCombatInputActivate => "PlayerCombatInputActivate",
            GameMessageId::PlayerCombatInputPosition => "PlayerCombatInputPosition",
            GameMessageId::MatchPostRoundInfoMsg => "MatchPostRoundInfoMsg",
            GameMessageId::MatchEndMatchMsg => "MatchEndMatchMsg",
            GameMessageId::ReceiveDamage => "ReceiveDamage",
            GameMessageId::ChangeCombatStatusEffect => "ChangeCombatStatusEffect",
            GameMessageId::PlayerAutoAttackStateChange => "PlayerAutoAttackStateChange",
            GameMessageId::PlayerChannelingStateChange => "PlayerChannelingStateChange",
            GameMessageId::CombatSwipeInfo => "CombatSwipeInfo",
            GameMessageId::CombatScreenInfo => "CombatScreenInfo",
            GameMessageId::EquipAbilitiesAndConsumables => "EquipAbilitiesAndConsumables",
            GameMessageId::SkipCurrentState => "SkipCurrentState",
            GameMessageId::PlayerManeuverStateChange => "PlayerManeuverStateChange",
            GameMessageId::InterruptAbility => "InterruptAbility",
            GameMessageId::StopAbility => "StopAbility",
            GameMessageId::LoadoutClientBackendSynchronized => "LoadoutClientBackendSynchronized",
            GameMessageId::RequestConsumeConsumable => "RequestConsumeConsumable",
            GameMessageId::PerformConsumeConsumable => "PerformConsumeConsumable",
            GameMessageId::PlayerStatsUpdate => "PlayerStatsUpdate",
            GameMessageId::DamageNegated => "DamageNegated",
            GameMessageId::ServerGcCount => "ServerGcCount",
            GameMessageId::RecordUrls => "RecordUrls",
            GameMessageId::PlayEmote => "PlayEmote",
            GameMessageId::PlayerEmoteStateChange => "PlayerEmoteStateChange",
            GameMessageId::SyncStartRecording => "SyncStartRecording",
            GameMessageId::PlayerDestroyedStatUpdate => "PlayerDestroyedStatUpdate",
            GameMessageId::ClientAppPaused => "ClientAppPaused",
            GameMessageId::ServerShutdown => "ServerShutdown",
            GameMessageId::PlayerPlayVFX => "PlayerPlayVFX",
            GameMessageId::MatchStateChangeRequest => "MatchStateChangeRequest",
            GameMessageId::MatchStateChangeAck => "MatchStateChangeAck",
            GameMessageId::RestoreActor => "RestoreActor",
            GameMessageId::CombatLog => "CombatLog",
            GameMessageId::ModifyAbilityCooldowns => "ModifyAbilityCooldowns",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_matches_defined_variants() {
        // Every member of the gate set must round-trip through the enum, and
        // every non-member must be rejected — i.e. the const array and the enum
        // are the same set (and thus identical to the Python frozenset).
        for b in 0u8..=255 {
            assert_eq!(
                is_game_message_id(b),
                GameMessageId::from_u8(b).is_some(),
                "gate/enum disagree for byte {b}"
            );
        }
        assert_eq!(GAME_MESSAGE_IDS.len(), 59);
        // Spot-check the gaps are excluded.
        for gap in [25u8, 62, 67, 68, 69] {
            assert!(!is_game_message_id(gap), "{gap} should not be a GameMessageId");
        }
    }

    #[test]
    fn known_opcodes() {
        assert_eq!(GameMessageId::from_u8(50), Some(GameMessageId::ReceiveDamage));
        assert_eq!(GameMessageId::from_u8(49), Some(GameMessageId::MatchEndMatchMsg));
        assert_eq!(GameMessageId::ReceiveDamage.as_u8(), 50);
        assert_eq!(GameMessageId::CombatLog.to_string(), "CombatLog");
    }
}
