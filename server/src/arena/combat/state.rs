//! Authoritative per-match combat state + the arena protocol enums.
//!
//! Enum discriminants are the on-wire values from `reference/il2cpp/dump.cs` /
//! `reference/il2cpp/arena-opcodes.json` and the field-level decode in the
//! capture repo's `docs/archive/arena-combat-reference.md`. Where an enum is only
//! partially mapped it is marked `// …` — extend as more values are confirmed.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

/// Max value of a **packed wire stat** — 10 bits each (Health/Stamina/Magicka pack
/// into the low 30 bits of the `ReceiveDamage` stats ULong). NOTE: the wire field is
/// a **fraction of max** (`STAT_MAX` = full), NOT raw HP — raw HP is hundreds-to-
/// thousands and ×3 in arena exceeds 10 bits. See docs/blades-combat-formulae.md §9.
pub const STAT_MAX: u16 = 1023;

/// Arena multiplies max HEALTH by this (`PvpDefaultSettings.CHEAT_BASE_HEALTH_MULTIPLIER
/// = 3`, dump 427012). Stamina/Magicka are NOT multiplied. See formulae doc §10.
pub const ARENA_HEALTH_MULTIPLIER: u32 = 3;

/// Round-wins needed to win the MATCH — best-of-3 (`MaxMatchRounds` = 3, s506 Match
/// propId8 / `messages::MATCH_MAX_ROUNDS`). First fighter to 2 round-wins ends the
/// match; before that, a round-ending death loops to the next round.
pub const ROUND_WINS_TO_WIN_MATCH: u8 = 2;

/// Approximate base max-Health for a level (UESP L50-era curve: 200 + 10/level). Our
/// build is L100 so this is representative until the real `PlayerStatsData` curve is
/// wired; validate magnitudes against captures (docs/blades-combat-formulae.md §9).
pub fn health_for_level(level: u16) -> u32 {
    200 + 10 * level.saturating_sub(1) as u32
}
/// Approximate Stamina/Magicka pool for a level (the player splits one per level).
pub fn pool_for_level(level: u16) -> u32 {
    200 + 5 * level.saturating_sub(1) as u32
}
/// Encode a raw pool value as its 10-bit wire fraction of max (`STAT_MAX` = full).
pub fn wire_fraction(cur: u32, max: u32) -> u16 {
    if max == 0 {
        return 0;
    }
    ((cur.min(max) as u64 * STAT_MAX as u64) / max as u64) as u16
}

// ---------------------------------------------------------------------------
// Shared protocol enums (NetObjectInfo + combat)
// ---------------------------------------------------------------------------

/// `NetRole` — who owns/authorities a net object. propId 2 of NetObjectInfo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NetRole {
    None = 0,
    Authority = 1,
    Simulated = 2,
    Autonomous = 3,
}

/// `NetObjectType` — propId 1 of NetObjectInfo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NetObjectType {
    Match = 54,
    Player = 55,
    Avatar = 56,
    Control = 57,
}

/// `ActiveSide` — guard / swipe side. `ReceiveDamage` propId 10.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ActiveSide {
    None = 0,
    Middle = 1,
    Left = 2,
    Right = 3,
}

/// `DamageSource` — `ReceiveDamage` propId 6. Observed 1–4 in captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DamageSource {
    None = 0,
    Attack = 1,
    Spell = 2,
    WeaponManeuver = 3,
    StatusEffect = 4,
    Trap = 5,
    Revenge = 6,
    AreaEffect = 7,
    ContinuousSpell = 8,
    EchoWeapon = 9,
    ContinuousAttack = 10,
    ShieldManeuver = 11,
}

/// `DamageType` — per-component damage type. `ReceiveDamage` damageByType[].type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DamageType {
    None = 0,
    Slashing = 1,
    Cleaving = 2,
    Bashing = 3,
    Fire = 4,
    Frost = 5,
    Shock = 6,
    Poison = 7,
    Stamina = 8,
    Magicka = 9,
    Health = 10,
}

/// `ActorStateType` — an actor's current combat animation/logic state
/// (`PlayerChannelingStateChange` stateId etc.).
///
/// **Fully mapped.** These are the client's own `ActorStateType.StateId` values,
/// transcribed verbatim from `reference/il2cpp/dump.cs` lines 340171–340200
/// (`public enum ActorStateType.StateId // TypeDefIndex: 6252`) in the
/// `blades-capture` repo. The seven discriminants that were already
/// capture-confirmed here before the transcription — `Idle`, `Channeling`,
/// `Staggered`, `Dialogue`, `Paralyzed`, `PlayerAutoAttack`, `Emote` — all match
/// the dump exactly, which is what makes the remaining values trustworthy enough
/// to put on the wire. `Blocking = 1` was a server-internal placeholder and turns
/// out to be the real wire value too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ActorStateType {
    Idle = 0,
    /// Shield up. Serialized in `PlayerBlockingStateChange` (gmid 41).
    Blocking = 1,
    /// Winding up a charged attack. Serialized in `PlayerChargingStateChange` (gmid 45).
    Charging = 2,
    Dead = 3,
    Channeling = 4,
    Staggered = 5,
    Moving = 6,
    MovingToPoint = 7,
    Dialogue = 8,
    Crafting = 9,
    PreFight = 10,
    /// Sidestep / reposition. Serialized in `PlayerStateChange` (gmid 39).
    Maneuver = 11,
    Notify = 12,
    /// `ActorParalyzedState` — the paralysed actor state. **StateId 13**
    /// (`dump.cs` 340018/340188; `arena-status-resistance-spec.md` §5.4). The victim's
    /// inputs are blocked for the `Paralyzed` status duration (3.1 s).
    Paralyzed = 13,
    /// Draining an enemy (life/magicka leech). `PlayerDrainingStateChange` (gmid 42).
    PlayerDraining = 14,
    /// An ability swing in progress. Distinct from `PlayerAutoAttack`.
    PlayerAttack = 15,
    /// Post-swing recovery — the window in which the player may NOT act again.
    /// `PlayerRecoveryStateChange` (gmid 44) is what tells the client when the
    /// ability buttons come back.
    PlayerRecovery = 16,
    /// The follow-through of a swing. `PlayerFollowThroughStateChange` (gmid 43).
    PlayerFollowThrough = 17,
    PlayerBreakingItem = 18,
    /// A basic (non-ability) swing. `PlayerAutoAttackStateChange` (gmid 52).
    PlayerAutoAttack = 19,
    EnemyNonLethal = 20,
    EnemyAttackAndMove = 21,
    EnemyCritterAttacking = 22,
    EnemyCritterStepBack = 23,
    EnemyCritterIdle = 24,
    OpponentChargedAttacking = 25,
    SocialInteraction = 26,
    OpponentVictory = 27,
    Emote = 28,
}

/// How many entries the client's `PvpPlayerStateHistory` ring retains. Capture-pinned:
/// the `retainedCount` byte at the head of propId 7 rises to 20 and then saturates,
/// and the widest ByteArray in the retail corpus is 23 bytes = 3 header + 20 entries.
pub const STATE_HISTORY_MAX: usize = 20;

/// One recorded `old → new` actor-state transition, queued on the fighter until the
/// resolver drains it into the matching s2c `PlayerStateChange`-family frame.
///
/// `from` is carried as well as `to` because leaving a state can be as meaningful as
/// entering one: `Blocking → anything` is what LOWERS the shield (gmid 41 with
/// `blocking=false`), and there is no other message that says it.
#[derive(Debug, Clone, PartialEq)]
pub struct StateTransition {
    pub from: ActorStateType,
    pub to: ActorStateType,
    /// The `PvpPlayerStateHistory` ring as it stood **at this transition**, already in
    /// wire layout for propId 7. Snapshotted rather than read at drain time because a
    /// tick can queue several transitions, and the newest history entry must equal the
    /// frame's own propId 6 — capture-pinned in 479/479 retail frames.
    pub history: Vec<u8>,
    /// Seconds the actor spent in `from` — the `_timeInPreviousState` float the
    /// family carries at propId 8. Captured at the moment of the transition, not at
    /// drain time, because the drain happens after `state_entered` was restamped.
    pub time_in_previous: f32,
}

/// `StatusEffectType` — combat status effects (`ChangeCombatStatusEffect`, op51 propId5).
/// Capture-decoded counts/durations in `arena-status-resistance-spec.md` §5.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum StatusEffectType {
    None = 0,
    Blocking = 1,
    Staggered = 3,
    /// Fire conditioning DoT (5 s window). [§5.3]
    Burning = 4,
    /// Frost conditioning DoT (5 s). [§5.3]
    Frozen = 5,
    /// Shock conditioning DoT ("Enervated"/Drained, 5 s). [§5.3]
    Enervated = 6,
    /// Poison conditioning DoT (2.52 / 4.89 / 5 s; 4.89 s = the Paralyze-spell poison). [§5.3]
    Poisoned = 7,
    /// `Paralyzed` — the un-breakable paralyse state (3.1 s = `ParalyzeAbility._duration`). [§5.4]
    Paralyzed = 9,
    StaggeredWeakness = 10,
    Dodging = 12,
    /// Ward negation buff (elemental-negation pool + armor). [§4.2]
    Ward = 15,
    /// Absorb negation buff (damage→heal pool). [§4.1]
    Absorb = 17,
    /// No HP regen while active (On Fire / conditioning). [status-resistance-spec §Mechanic-2]
    BlockHealthRegen = 50,
    BlockStaminaRegen = 51,
    /// No magicka regen while active (Enervated). [status-resistance-spec §Mechanic-2]
    BlockMagickaRegen = 52,
    /// Resist-Elements 4-tuple (FireResistance 60 … PoisonResistance 63, 11.5 s). [§4.3]
    FireResistance = 60,
    FrostResistance = 61,
    ShockResistance = 62,
    PoisonResistance = 63,
    // …
}

/// `BLOCK_OPTIMAL_TIME` (dump.cs 427014): how long (seconds) the shield can be held
/// at OPTIMAL efficiency before degrading to LATE.
pub const BLOCK_OPTIMAL_TIME_SECS: f32 = 2.0;

/// Cooldown (seconds) after dropping the block before a new OPTIMAL window can
/// begin. Re-raising within this window starts as LATE, not OPTIMAL.
///
/// **Phase 3.5:** this is `PlayerCombatParameters.postOptimalBlockResetTime`
/// (**1.4 s**), replacing the dump's `OPTIMAL_BLOCK_RECOVERY_TIME = 0.8 s`. The
/// shipped player-combat asset is the authority for a *player* re-raising a guard;
/// `PvpDefaultSettings` 0.8 was a server-cheat default.
pub const OPTIMAL_BLOCK_RECOVERY_SECS: f32 =
    super::gamedata::combat_params::POST_OPTIMAL_BLOCK_RESET_TIME;

/// The block phase for a defending fighter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockPhase {
    /// Guard just raised — within `BLOCK_OPTIMAL_TIME_SECS` (phys ×0, elem ×0.5).
    Optimal,
    /// Guard held too long — after `BLOCK_OPTIMAL_TIME_SECS` (phys ÷1.6, elem ÷1.23).
    Late,
}

/// The status condition an elemental [`DamageType`] accumulates toward (the
/// conditioning rule, §5). `Fire→Burning`, `Frost→Frozen`, `Shock→Enervated`,
/// `Poison→Poisoned`; non-elemental types have no condition.
pub fn condition_for_element(t: DamageType) -> Option<StatusEffectType> {
    Some(match t {
        DamageType::Fire => StatusEffectType::Burning,
        DamageType::Frost => StatusEffectType::Frozen,
        DamageType::Shock => StatusEffectType::Enervated,
        DamageType::Poison => StatusEffectType::Poisoned,
        _ => return None,
    })
}

/// `DamageNegationSource` (dump.cs 546390) — which pool ate a hit. Drives op66
/// `DamageNegated`. [`arena-status-resistance-spec.md` §4.5]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DamageNegationSource {
    None = 0,
    Dodge = 1,
    Absorb = 2,
    Ward = 3,
    Breath = 4,
    Immunity = 5,
}

/// The match flow-control state — driven server-side, echoed by the client, sent
/// as a stateName string on the flow-controller net object (see module docs).
/// These are the literal wire strings observed in session 293.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowState {
    /// Pre-match: peers connected, not yet greeted into the match.
    Connecting,
    /// Internal (no wire stateName): the spawn/profile/channeling burst has been sent
    /// and we hold before `BackendMatchCreated`. Retail staggers the round-start ~4s
    /// (s506: spawns 05:05:36 → BackendMatchCreated 05:05:40); the client uploads its
    /// loadout (PlayerLoadoutReady) during this gap. Announcing the match in the same
    /// tick as the spawns preempts that handshake and hangs the client at "Connecting".
    Spawning,
    /// `BackendMatchCreated` — the match exists; loadout/spawn happen around here.
    BackendMatchCreated,
    /// `StateTimeout` — periodic heartbeat while a phase runs (the dominant
    /// s2c flow message; emitted on the tick).
    StateTimeout,
    /// `NextState` — advance to the next round/phase.
    NextState,
    /// `RoundEnd` — a round concluded.
    RoundEnd,
    /// Match concluded (no more rounds).
    Finished,
}

/// `MatchState.State` (`dump.cs:591661`, TypeDefIndex 12637) — the client's
/// authoritative match state machine. **It is NOT driven by the op79 `stateName`
/// trigger strings** (those drive the separate `PvpClientFlowController`). It is a
/// **replicated property (propId 5) of the type-54 Match net-object** the server
/// spawns at round start: the client's `Match.OnObjectPropertiesChanged` reads it
/// and fires `OnMatchStateChanged`, and it binds the local/opponent `PvpPlayer`
/// during `WaitingForPlayers`(3) / `InitialPlayerSetup`(4). Capture-proven from
/// s506: the Match object (obj 123) is spawned with propId5 = 3 and advanced via
/// op55 (0x35) property updates 3→4→5→6→7→11 (the exact enum order, timeouts in
/// propId6). Spawning the object with state 5 (as the old per-fighter "ability"
/// spawn did) makes the client jump Idle→5, skip 3/4, and never bind its players
/// (`HasLocalPlayer`=0) — the "Match net-object frozen at BackendMatchCreation" bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MatchState {
    Idle = 0,
    ConnectedMatch = 1,
    ActiveMatch = 2,
    WaitingForPlayers = 3,
    InitialPlayerSetup = 4,
    BackendMatchCreation = 5,
    OpponentFoundFeedback = 6,
    PreMatch = 7,
    ChooseLoadout = 8,
    AwaitingClientBackendSynchronization = 9,
    SynchronizingLoadout = 10,
    OpponentShowcase = 11,
    PreRound = 12,
    InRound = 13,
    PostRound = 14,
    Victory = 15,
    PostMatch = 16,
    BackendMatchEnd = 17,
    FinalizingMatch = 18,
    DisconnectingPlayersAfterMatch = 19,
}

impl FlowState {
    /// The exact ASCII stateName string on the wire, or `None` for the synthetic
    /// pre/post states that aren't themselves a wire string.
    pub fn wire_name(self) -> Option<&'static str> {
        Some(match self {
            FlowState::BackendMatchCreated => "BackendMatchCreated",
            FlowState::StateTimeout => "StateTimeout",
            FlowState::NextState => "NextState",
            FlowState::RoundEnd => "RoundEnd",
            FlowState::Connecting | FlowState::Spawning | FlowState::Finished => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Packed stats (ReceiveDamage propId 4/5)
// ---------------------------------------------------------------------------

/// `ReceiveDamage` propIds 4/5 pack a player's pools + a sequence id into one
/// ULong. **Layout (verified against captures, s293):** the **HIGH 32 bits** hold
/// the stat word `Health | Stamina<<10 | Magicka<<20` (10 bits each, `STAT_MAX`),
/// and the **LOW 32 bits** hold the `sequenceId` (a small rising counter). (The
/// first-pass RE + the archived `arena-combat-reference.md` had these halves
/// backwards — a full actor reads 1023 from the HIGH half, not the low.)
pub struct PackedStats;

impl PackedStats {
    pub fn pack(health: u16, stamina: u16, magicka: u16, seq: u32) -> u64 {
        let h = (health.min(STAT_MAX) as u64) & 0x3ff;
        let s = (stamina.min(STAT_MAX) as u64) & 0x3ff;
        let m = (magicka.min(STAT_MAX) as u64) & 0x3ff;
        let stats = h | (s << 10) | (m << 20);
        (stats << 32) | (seq as u64) // stats in the HIGH 32, sequence id in the LOW 32
    }

    /// Returns `(health, stamina, magicka, seq)`.
    pub fn unpack(v: u64) -> (u16, u16, u16, u32) {
        let stats = (v >> 32) as u32;
        let health = (stats & 0x3ff) as u16;
        let stamina = ((stats >> 10) & 0x3ff) as u16;
        let magicka = ((stats >> 20) & 0x3ff) as u16;
        let seq = (v & 0xffff_ffff) as u32;
        (health, stamina, magicka, seq)
    }
}

// ---------------------------------------------------------------------------
// Loadout (initialized from the imported character; refined in combat/loadout.rs)
// ---------------------------------------------------------------------------

/// High-level ability classification for abilities that need special server-side
/// handling beyond the generic spell-damage path. Set by `loadout::from_character`
/// when the imported character's ability template UUID matches a known class
/// (`ward_ability_uuids`, `resist_elements_ability_uuids` in loadout.rs). Keeps the
/// generic damage path working without game-data for unrecognized abilities.
/// **Phase 3.11:** derived from the full 63-ability shipped table
/// (`loadout::ability_tag_for_template`), not a single hardcoded UUID prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AbilityTag {
    #[default]
    /// Unrecognised / passive — handled by the default `resolve_ability` path.
    Generic,
    /// `WardAbility` (Ward / Spellbreaker): apply a Ward negation pool + armor.
    Ward,
    /// `AbsorbAbility`: a negation pool that HEALS back what it eats
    /// (`_maximumAmountAbsorbed` + `_restorationFactor`).
    Absorb,
    /// `ResistElementsAbility`: apply the 4-tuple elemental resistance.
    ResistElements,
    /// `ParalyzeAbility`: direct damage + the paralyse threshold/duration.
    Paralyze,
    /// A direct-damage spell with a shipped `_damage` and `damage_type`.
    Damage,
    /// A stamina-cost maneuver (`_parameters.one_handed_multiplier` scales the swing).
    Maneuver,
    /// A passive perk — never activates.
    Perk,
}

/// One equipped ability: its instance UUID (as referenced by
/// `RequestExecuteAbility`) and its level (drives scaling/cooldown).
#[derive(Debug, Clone)]
pub struct EquippedAbility {
    pub instance_uuid: String,
    pub level: u8,
    pub tag: AbilityTag,
}

/// The weapon's base damage profile (per-type) + its shipped cadence/block stats.
///
/// **Phase 3.1/3.2/3.12:** every field below is now resolved from the equipped
/// item's template via `gamedata::weapon(...)`; only fighters whose item does not
/// resolve (bots / starter) fall back to the UESP surface.
#[derive(Debug, Clone, Default)]
pub struct WeaponProfile {
    pub primary_type: Option<DamageType>,
    /// Base damage per type before swing/ability/enchant factors, already including
    /// the item's `tempering_level` bonus (`tables::tempering_bonus`).
    pub base_by_type: Vec<(DamageType, f32)>,
    /// Weapon weight class — drives the combo/crit ramp (`damage::combo_factor`).
    /// Resolved from the template's `weapon_class`; `None` ⇒ the model default (Light).
    pub weight: Option<crate::arena::combat::tables::Weight>,
}

/// The shipped `WeaponTemplateList` row backing a fighter's weapon, when the equipped
/// item resolved (Phase 3.1/3.12).
///
/// **Why this hangs off [`Loadout`] and not [`WeaponProfile`]:** `WeaponProfile` is
/// constructed as a struct literal in `engine.rs` (a file this change does not own),
/// so widening it would break that call site. The three-field profile stays
/// source-compatible and the item stats live one level up.
pub type WeaponTemplate = &'static crate::arena::combat::gamedata::WeaponStats;

/// A fighter's combat-relevant equipment, derived from the imported character.
#[derive(Debug, Clone, Default)]
pub struct Loadout {
    /// Character level — drives max-Health/Stamina/Magicka (`health_for_level`).
    pub level: u16,
    pub abilities: Vec<EquippedAbility>,
    pub weapon: WeaponProfile,
    /// The shipped weapon template the equipped item resolved to (Phase 3.1). `None`
    /// ⇒ this loadout used the UESP fallback surface (bot / no inventory).
    pub weapon_template: Option<WeaponTemplate>,
    pub has_shield: bool,
    /// Enchant `(damage_type, tier)` contributions, kept for provenance/logging.
    /// The MAGNITUDE now lives in [`Self::enchant_damage`] — the shipped `_value`
    /// curve is per-family and convex, so `tier` alone cannot produce it.
    pub enchants: Vec<(DamageType, u8)>,
    /// Attacker-side **Armor Piercing Rating**
    /// (`ArmorPiercingPhysicalPropertyLogic`) — subtracted from the defender's
    /// Armor Rating before the physical reduction. [Phase 3.3]
    pub armor_piercing_rating: f32,
    /// Attacker-side `Fortify <Element> Damage` — a 0..1 fraction per element that
    /// raises that element track's amplification ceiling. [Phase 3.6]
    pub element_fortify: Vec<(DamageType, f32)>,
    /// Display name + character UUID for the round-start op50 spawn. Empty for the
    /// starter loadout (no character row); set by `loadout::from_character` + the
    /// matchmaker's character load.
    pub display_name: String,
    pub character_uuid: String,
    /// The two op54 round-start PROFILE JSON blobs (gear + full character), serialized
    /// from the stored character by the matchmaker; empty for the starter loadout.
    pub profile_equipped_json: String,
    pub profile_character_json: String,

    // --- Rating-derived defence (Phase 3.3/3.4/3.5) ---
    /// Summed **Armor Rating** of every equipped armor piece
    /// (`gamedata::armor_rating`). Reduces PHYSICAL damage only, via
    /// `tables::armor_reduction` (`rating × reductionPerArmorRating`, capped at
    /// `maximumArmorReduction`). 0.0 for a fighter with no resolvable armor.
    pub armor_rating: f32,
    /// Summed **Block Rating** contributed by the equipped weapon + shield
    /// (`blockBase`). Feeds `tables::block_reduction`.
    pub block_rating: f32,
    /// The shield's `optimalBlockBoost` (1.0 when no shield / unresolved).
    pub shield_optimal_block_boost: f32,
    /// **Elemental Resistance Piercing RATING** (not a fraction) contributed by the
    /// attacker's enchants — subtracted from the defender's resistance rating before
    /// the reduction is computed. [Phase 3.4]
    pub elem_resist_piercing_rating: f32,
    /// The caster's Paralyze ability rank (0 = not equipped) — selects the shipped
    /// `_damageToCauseParalyze` / `_duration` row. [Phase 3.9]
    pub paralyze_rank: u8,
    /// The equipped WEAPON's `optimalBlockBoost` (1.0 when unresolved).
    pub weapon_optimal_block_boost: f32,

    // --- Defensive / offensive enchant-derived fields (status-resistance-spec §2.5) ---
    /// Summed **Resistance Rating** per `DamageType` (armor "Resist X" enchants +
    /// perks). With `reductionPerResistanceRating = 1.0` a rating point is a flat
    /// damage point, which is exactly how the shipped enemy assets read it
    /// (`Nascent Flame Atronach resistances.Fire = 65.28`). [Phase 3.4]
    pub resistances: Vec<(DamageType, f32)>,
    /// Summed flat weakness per type (a flat damage INCREASE). Usually empty in PvP.
    pub weaknesses: Vec<(DamageType, f32)>,
    /// Attacker-side **Elemental Resistance Piercing** (fraction 0..1): the defender's
    /// elemental resistance is scaled by `(1 − elem_resist_piercing)` before applying.
    pub elem_resist_piercing: f32,
    /// Per-condition threshold BUMP (fraction): "Fortify Poisoned/Burning/Frozen/
    /// Enervated" raise that condition's land threshold by this fraction of max HP.
    pub status_resist: Vec<(StatusEffectType, f32)>,
    /// "Shorten/Extend Elemental Statuses" → multiply status `_duration` by this (1.0 =
    /// none). Parsed but not yet applied to DoT timers (informational).
    pub status_dur_mult: f32,
}

impl Loadout {
    /// Commit-to-commit swing cadence (Phase 3.12): the equipped template's own
    /// `attackDelay + recoveryTime`, floored at `globalMinimumAttackDelay`; the
    /// weight-class fallback when no template resolved.
    pub fn swing_interval(&self) -> std::time::Duration {
        use crate::arena::combat::tables;
        match self.weapon_template {
            Some(w) => tables::swing_interval_for_weapon(w),
            None => tables::fallback_swing_interval(self.weapon.weight.unwrap_or(tables::Weight::Light)),
        }
    }
}

// ---------------------------------------------------------------------------
// Active status effect on a fighter (DoT / buff / debuff)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ActiveEffect {
    pub effect: StatusEffectType,
    pub damage_type: DamageType,
    /// Per-tick magnitude (DoT) or flat magnitude (buff).
    pub value: f32,
    /// `_percentHealthDamage × maxHP` per tick (DoT); 0.0 for non-DoT effects.
    /// Game-data-driven — the observed s506 range is 1.25–7.73 damage/tick.
    /// **CALIBRATION FLAG**: the exact `_percentHealthDamage` requires the game's
    /// Excel data. Current default: 0.003 of max HP per tick (≈ 3.87/tick at L86
    /// arena×3 HP ≈ 1290 maxHP — the dominant s506 Poison DoT value).
    pub per_tick_damage: f32,
    pub expires_at: Instant,
    pub last_tick: Instant,
    /// True for a Resist-Elements transient resistance — these are carried in
    /// `ActiveEffect` rather than the permanent `Loadout.resistances` so they
    /// auto-expire and are cleaned up without touching the loadout.
    pub is_transient_resist: bool,
}

// ---------------------------------------------------------------------------
// Per-fighter authoritative state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Fighter {
    pub slot: usize,
    /// The Avatar net object id the server assigns and addresses in messages.
    pub net_object_id: i32,
    /// The Player net object id (distinct from the Avatar id) — addressed by the
    /// round-start op50 Player spawn. Allocated by `MatchInstance::new`.
    pub player_net_object_id: i32,
    /// The type-54 "Match/ability" net object id (op50 spawn + op53 channeling at
    /// round-start). Allocated by `MatchInstance::new`.
    pub ability_net_object_id: i32,
    /// Raw pools (hundreds-to-thousands; Health is ×3 in arena). The WIRE packs a
    /// FRACTION of max — see `packed_stats` / `wire_fraction`, not these raw values.
    pub health: u32,
    pub stamina: u32,
    pub magicka: u32,
    pub max_health: u32,
    pub max_stamina: u32,
    pub max_magicka: u32,
    /// hi32 of the packed-stats ULong; bumped on each `ReceiveDamage`.
    pub stats_seq: u32,
    pub loadout: Loadout,
    /// Ability instance UUID → time it comes off cooldown.
    pub cooldowns: HashMap<String, Instant>,
    pub effects: Vec<ActiveEffect>,
    /// The fighter's current animation/logic state.
    ///
    /// **PRIVATE ON PURPOSE.** Every write must go through
    /// [`Fighter::set_actor_state`] so the transition lands in
    /// [`Fighter::pending_state_changes`] and the resolver can put it on the wire.
    /// This field used to be `pub` and was assigned in a dozen scattered places,
    /// none of which told the client anything — which is precisely why nothing
    /// animated. Read it with [`Fighter::actor_state`].
    actor_state: ActorStateType,
    /// Actor-state transitions that have happened since the resolver last drained
    /// this fighter, oldest first. Drained by
    /// [`super::resolve::drain_state_changes`] at the end of every input/tick and
    /// turned into the `PlayerStateChange`-family s2c frames (39/41/42/43/44/52).
    ///
    /// A per-tick outbox rather than an emit-at-the-callsite, because four of the
    /// writers (`damage::block_outcome`, `reconcile_block`, …) run deep inside the
    /// damage model with no viewer list and no message context in scope.
    pending_state_changes: Vec<StateTransition>,
    /// Actor states this fighter should enter LATER, `(when, state)`, kept sorted by
    /// `when`. Applied by [`Fighter::reconcile_scheduled_states`] on the next
    /// input/tick at or after the instant.
    ///
    /// A swing is not one state — retail walks the actor through
    /// `PlayerAutoAttack → PlayerFollowThrough → PlayerRecovery → Idle` over the
    /// weapon's own `attackDelay + recoveryTime`. Firing all four the instant the
    /// swing resolves would make the client flash through the animation instead of
    /// playing it. The arena loop ticks every 2 ms, so scheduling is accurate to
    /// far finer than the ~230 ms shortest phase.
    scheduled_states: Vec<(Instant, ActorStateType)>,
    /// The last [`STATE_HISTORY_MAX`] states this actor entered, oldest first — the
    /// `PvpPlayerStateHistory` ring the family carries at propId 7.
    state_history: VecDeque<ActorStateType>,
    /// How many transitions this actor has made this round in total, including the
    /// ones that have aged out of [`Self::state_history`]. The wire `firstIndex` is
    /// `transitions_total - state_history.len()`.
    transitions_total: u32,
    pub state_entered: Instant,
    /// Slot of the implicit arena target (the opponent) for `RequestExecuteAbility`.
    pub arena_target: usize,
    pub blocking_side: ActiveSide,
    /// While set and in the future, this fighter is BLOCKING (guard up) until the
    /// instant — incoming hits are reduced/negated per `damage::block_outcome`. Set
    /// when the client sends `PlayerBlockingStateChange` (41); auto-expires after the
    /// block window (`resolve::BLOCK_WINDOW`, the dump's `BLOCK_OPTIMAL_TIME`). `None`
    /// (or past) ⇒ not blocking. Expiry is reconciled into `actor_state` on each
    /// input/tick (where `now` is available).
    pub blocking_until: Option<Instant>,
    /// The `Instant` when the current guard was raised (for OPTIMAL→LATE timeout).
    /// `None` when not blocking. Set alongside `blocking_until` on op41.
    pub block_raised_at: Option<Instant>,
    /// The `Instant` the last block was DROPPED (for the OPTIMAL-recovery cooldown).
    /// Guards raised within `OPTIMAL_BLOCK_RECOVERY_SECS` after dropping start as LATE.
    pub last_block_dropped_at: Option<Instant>,
    /// Time of this fighter's last landed swing (combat throttle / swing cadence).
    pub last_swing: Option<Instant>,
    /// The `ActiveSide` classified at the moment the attack button went DOWN, i.e.
    /// the side the wind-up (gmid 45 `Charging`) is announced with.
    ///
    /// Retail carries ONE side across all four beats of a swing — `45.prop9 ==
    /// 52.prop9` in 593 of 593 captured pairs — so the charge and the swing must
    /// agree. We classify at press for the charge and again at release for the swing
    /// (release-classification is what the damage model is calibrated on), and use
    /// this as the release fallback when the pointer sample has gone stale. They can
    /// still differ if the finger crosses the screen midpoint mid-hold; retail
    /// evidently latches at press, and matching that exactly would change the
    /// damage-side behaviour, which is not this change's job.
    pub charge_side: Option<ActiveSide>,
    /// For a BOT: when its already-started wind-up should land as an actual swing.
    /// A bot has no button to press, so `on_tick` enters `Charging` first and resolves
    /// the swing `CHARGE_WINDUP` later — otherwise the charge and the swing would be
    /// drained in the same tick and the client would flash through the animation.
    pub bot_swing_at: Option<Instant>,
    /// Server-side timestamp when this fighter last pressed the attack button (op46
    /// `_held=1`). Used with the release timestamp (op46 `_held=0`) to compute the
    /// server-measured hold duration for the held-charge crit gate (bug 4).
    /// `None` ⇒ no press in progress (e.g. bot swings, or between attacks).
    pub charge_press_at: Option<Instant>,
    /// **Combo state** (`docs/arena-combat-reproduction-spec.md` §4.2). The number of
    /// uninterrupted, **alternating-side** swings chained so far — drives the combo
    /// ramp (`damage::combo_factor`: ×1.0 → ×1.45 → … → ~×4.12 for a Light weapon).
    /// Incremented on a normal Left/Right swing that alternates vs `last_combo_side`
    /// within the combo window; RESET to 0 on a non-alternating/late swing, an optimal
    /// block, a `Middle` maneuver, and at round start. Mirrors the client's
    /// `AttackerStateData._comboCount`/`IncrementCombo`/`ResetCombo` (dump.cs).
    pub combo_count: u32,
    /// The `ActiveSide` of this fighter's last combo-counting swing (Left/Right), so
    /// the next swing can tell an *alternating* chain (combo++) from a repeat (reset).
    /// `None` at round start / after a reset.
    pub last_combo_side: ActiveSide,

    // --- Phase 4.1: client pointer geometry (`PlayerCombatInputPosition`, gmid 47) ---
    /// Most recent **normalised screen X** the client reported (gmid 47 propId 4,
    /// range ~0.03–1.0). This is the field the swing-side classifier reads: prod
    /// ground truth separates Left (median x 0.213) from Right (median x 0.800)
    /// cleanly on this axis. `None` until the client sends its first pointer sample
    /// (bots, and clients that stream nothing, stay `None` forever — the classifier
    /// then falls back to the synthetic alternation).
    pub last_input_x: Option<f32>,
    /// Most recent normalised screen Y (gmid 47 propId 5). Recorded for completeness
    /// / future vertical gestures; it carries **no side signal** (prod medians 0.529
    /// Left vs 0.497 Right — indistinguishable).
    pub last_input_y: Option<f32>,
    /// When [`Self::last_input_x`] was recorded. A sample older than
    /// `SIDE_CLASSIFY_SAMPLE_TTL` is treated as stale and ignored.
    pub last_input_at: Option<Instant>,
    /// The **client-reported** charge/hold duration in seconds (gmid 46 propId 5, and
    /// the same value latched into gmid 47 propId 7).
    ///
    /// **TELEMETRY ONLY — never authoritative.** This is client-authored and therefore
    /// trivially spoofable (a modified client could report a full 1.2 s charge on every
    /// tap and crit every swing). The crit gate uses the *server*-measured hold
    /// (`charge_press_at` → release). This field exists so the two can be compared and
    /// a divergence logged.
    pub last_client_charge: Option<f32>,
    /// The client's `_isWithinBlockZone` flag from the last `PlayerCombatInputActivate`
    /// (gmid 46 propId 6). Recorded for telemetry; it does **not** gate swings — see
    /// the note in `resolve::on_c2s_input`.
    pub last_input_block_zone: Option<bool>,

    // --- Conditioning / status-effect machinery (status-resistance-spec §5) ---
    /// Sliding per-element damage window: `DamageType → [(amount, recorded_at)]`. Each
    /// inbound elemental component (post-block/resist/negate) is pushed here; entries
    /// age out after [`DAMAGE_HISTORY_WINDOW`]. A condition LANDS when the live sum for
    /// an element crosses its threshold (`CheckStatusEffectApplication`). Cleared on
    /// round reset (`ClearDamageHistory`).
    pub damage_history: HashMap<DamageType, Vec<(f32, Instant)>>,
    /// Whether this fighter CAN be paralysed (`Actor.CanBeParalyzed`). True for players;
    /// most bosses set `_innateImmunityParalyze`. [§5.4]
    pub can_be_paralyzed: bool,
    /// Active negation pools (Ward/Absorb/Dodge), drained per hit before damage lands.
    pub negation_pools: Vec<NegationPool>,
    /// Transient per-type flat resistances from Resist-Elements casts.  These are held
    /// separately from `loadout.resistances` so they expire cleanly without modifying the
    /// loadout. Drained by `transient_resistance_against` which is called from the damage
    /// pipeline AFTER block (same insertion point as loadout resistances). Duration = 11.5s
    /// (`ResistElementsAbility._resistanceDuration` from multi-session op51 analysis).
    pub transient_resistances: Vec<(DamageType, f32, Instant)>, // (type, flat_amount, expires_at)
    /// While set and in the future this fighter is STAGGERED
    /// (`CombatParameters.baseStaggerDuration` 1.5 s): inputs are dropped, exactly
    /// like `Paralyzed`, and the actor-state is `Staggered`. [Phase 3.13]
    pub staggered_until: Option<Instant>,
    /// Consumables used in the CURRENT round — gated by
    /// [`CONSUMABLES_PER_ROUND`] (1). Reset by `reset_fighters_for_next_round`.
    /// [Phase 4.3]
    pub consumables_used: u32,
    /// The consumable item UUID this fighter has equipped, as declared by its own
    /// `EquipAbilitiesAndConsumables` (56) upload (`{4:String consumableUuid ·
    /// 5:Int charges}`). It is the ONLY source of the UUID the server must echo in
    /// `PerformConsumeConsumable` (64) when the client requests a potion — the
    /// request (63) itself carries no payload. `None` until the client uploads its
    /// loadout. [Phase 4.3 wire trigger]
    pub equipped_consumable: Option<String>,
    /// How long the CURRENT paralysis lasts (the casting rank's shipped `_duration`);
    /// read by `resolve::reconcile_paralysis`. [Phase 3.9]
    pub paralyze_secs: f32,
}

/// A damage-negation pool (Ward/Absorb/Dodge) on a fighter — a quantity of
/// HP-equivalent that eats incoming damage until depleted or expired.
/// [`arena-status-resistance-spec.md` §4]
#[derive(Debug, Clone)]
pub struct NegationPool {
    pub source: DamageNegationSource,
    /// Remaining HP-equivalent the pool can still negate.
    pub remaining: f32,
    pub expires_at: Instant,
    /// Absorb heals the caster by `negated × restoration_factor` (≈1.0 = "100% heal");
    /// 0 for Ward/Dodge (pure negation, no heal-back). [§4.1]
    pub restoration_factor: f32,
}

/// The sliding damage-history window length (`ElementalStatusEffectData._duration` ≈ 5 s
/// — the conditioning window). [`arena-status-resistance-spec.md` §5.1]
pub const DAMAGE_HISTORY_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// `_healthPercentToCauseStatus` — fraction of MAX HP of accumulated [element] damage
/// (in the window) that LANDS the elemental condition. **Now read from the shipped
/// `CombatParameters.elemental_status_data`** (0.25, identical for all four elements).
/// Arena triples max HP, so ~3× raw damage is needed. [Phase 3.8]
pub const HEALTH_PERCENT_TO_CAUSE_STATUS: f32 =
    super::gamedata::combat_params::HEALTH_PERCENT_TO_CAUSE_STATUS;

/// `CombatParameters.baseStaggerDuration` — how long a staggered actor is locked out.
/// [Phase 3.13]
pub const BASE_STAGGER_DURATION_SECS: f32 = super::gamedata::combat_params::BASE_STAGGER_DURATION;

/// `CombatParameters.criticalHealthThreshold` — health **percentage** (0..100) below
/// which a fighter is "critical" (drives the potion prompt + `Fortify Health
/// Regeneration At Critical Health`). [Phase 3.13]
pub const CRITICAL_HEALTH_THRESHOLD_PCT: f32 =
    super::gamedata::combat_params::CRITICAL_HEALTH_THRESHOLD;

/// `PvpParameters.consumablesPerRound` — consumables a fighter may use per round.
/// [Phase 4.3]
pub const CONSUMABLES_PER_ROUND: u32 = super::gamedata::combat_params::CONSUMABLES_PER_ROUND;

/// The **absolute** accumulated-poison damage (inside the sliding window) that lands
/// `Paralyzed`, from `ParalyzeAbility`'s per-rank `_damageToCauseParalyze`
/// (**32.7 @ R1**, 37.63 @ R2, 43.77 @ R3 …).
///
/// **Phase 3.9 correction:** this replaces `PARALYZE_POISON_THRESHOLD_FRACTION = 0.45`
/// (a fraction of max HP → 1417 damage at L86 arena HP, ~43× the shipped value). The
/// shipped number is an absolute damage figure, so paralyse lands off a single strong
/// poison hit — which is what a Paralyze spell is supposed to do.
///
/// `rank` is the caster's Paralyze rank; when the attacker has no Paralyze ability the
/// R1 value is used as the generic poison→paralyse threshold.
pub fn paralyze_damage_threshold(rank: u8) -> f32 {
    super::gamedata::ability_rank_clamped(super::gamedata::ids::PARALYZE, rank.max(1) as u16)
        .and_then(|r| r.damage_to_cause_paralyze())
        .unwrap_or(32.7)
}

/// The shipped `Paralyze` per-rank `_duration` (**2.0 s @ R1**, 2.1 @ R2, 2.2 @ R3 …).
/// Replaces the invented `PARALYZE_DURATION_SECS = 3.1`. [Phase 3.9]
pub fn paralyze_duration_secs(rank: u8) -> f32 {
    super::gamedata::ability_rank_clamped(super::gamedata::ids::PARALYZE, rank.max(1) as u16)
        .and_then(|r| r.duration())
        .unwrap_or(2.0)
}

impl Fighter {
    pub fn new(slot: usize, net_object_id: i32, loadout: Loadout, now: Instant) -> Self {
        // Raw pools from the character's level. Arena triples HEALTH only
        // (`ARENA_HEALTH_MULTIPLIER`); Stamina/Magicka are not multiplied.
        let max_health = health_for_level(loadout.level) * ARENA_HEALTH_MULTIPLIER;
        let max_stamina = pool_for_level(loadout.level);
        let max_magicka = pool_for_level(loadout.level);
        Fighter {
            slot,
            net_object_id,
            player_net_object_id: 0, // assigned by MatchInstance::new
            ability_net_object_id: 0, // assigned by MatchInstance::new
            health: max_health,
            stamina: max_stamina,
            magicka: max_magicka,
            max_health,
            max_stamina,
            max_magicka,
            stats_seq: 0,
            loadout,
            cooldowns: HashMap::new(),
            effects: Vec::new(),
            // Construction, not a transition — nothing to tell a client that has no
            // avatar yet, so the field is set directly rather than via the mutator.
            actor_state: ActorStateType::Idle,
            pending_state_changes: Vec::new(),
            scheduled_states: Vec::new(),
            state_history: VecDeque::new(),
            transitions_total: 0,
            state_entered: now,
            arena_target: 1 - slot.min(1), // 2-player: the other slot
            blocking_side: ActiveSide::None,
            blocking_until: None,
            block_raised_at: None,
            last_block_dropped_at: None,
            last_swing: None,
            charge_press_at: None,
            charge_side: None,
            bot_swing_at: None,
            combo_count: 0,
            last_combo_side: ActiveSide::None,
            last_input_x: None,
            last_input_y: None,
            last_input_at: None,
            last_client_charge: None,
            last_input_block_zone: None,
            damage_history: HashMap::new(),
            can_be_paralyzed: true, // players can be paralysed (vs boss innate immunity)
            negation_pools: Vec::new(),
            transient_resistances: Vec::new(),
            staggered_until: None,
            consumables_used: 0,
            equipped_consumable: None,
            paralyze_secs: paralyze_duration_secs(1),
        }
    }

    /// This fighter's current actor state. The only way to read the private field.
    pub fn actor_state(&self) -> ActorStateType {
        self.actor_state
    }

    /// **The single seam for every actor-state change.** Records the `old → new`
    /// transition on [`Self::pending_state_changes`] and restamps
    /// [`Self::state_entered`]; a no-op when the state is already `next`.
    ///
    /// The resolver drains the queue at the end of each input/tick and puts each
    /// transition on the wire ([`super::resolve::drain_state_changes`]). Emitting
    /// here instead — at each of the dozen call sites — is exactly how the previous
    /// attempt would have missed one; the queue means a writer cannot forget.
    ///
    /// No-op on an unchanged state because retail does not spam a state change per
    /// tick: s503 sent 330 `PlayerAutoAttackStateChange` against 894 `ReceiveDamage`.
    pub fn set_actor_state(&mut self, next: ActorStateType, now: Instant) {
        if self.actor_state == next {
            return;
        }
        let from = self.actor_state;
        let time_in_previous = self.time_in_state(now);
        self.actor_state = next;
        self.state_entered = now;
        // The ring already contains the state being ENTERED — capture-pinned: the
        // last history byte equals propId 6 in all 479 decoded retail frames. So it is
        // updated BEFORE the snapshot is taken.
        if self.state_history.len() == STATE_HISTORY_MAX {
            self.state_history.pop_front();
        }
        self.state_history.push_back(next);
        self.transitions_total = self.transitions_total.saturating_add(1);
        self.pending_state_changes.push(StateTransition {
            from,
            to: next,
            history: self.packed_state_history(),
            time_in_previous,
        });
    }

    /// `PvpPlayerStateHistory` packed for propId 7, exactly as retail lays it out:
    ///
    /// ```text
    /// [u8 retainedCount] [u16-LE firstIndex] [retainedCount × u8 stateId]   (oldest → newest)
    /// ```
    ///
    /// `firstIndex` is the index of the oldest retained transition within the round's
    /// whole transition stream, so `firstIndex + retainedCount` is the running total.
    /// Capture-pinned against 479 retail frames: 479/479 conform, every history byte
    /// is a valid `ActorStateType`, and the newest entry always equals the frame's
    /// propId 6.
    pub fn packed_state_history(&self) -> Vec<u8> {
        let count = self.state_history.len();
        let first_index = (self.transitions_total as usize).saturating_sub(count) as u16;
        let mut out = Vec::with_capacity(3 + count);
        out.push(count as u8);
        out.extend_from_slice(&first_index.to_le_bytes());
        out.extend(self.state_history.iter().map(|s| *s as u8));
        out
    }

    /// Take everything queued since the last drain, leaving the queue empty.
    pub fn take_state_changes(&mut self) -> Vec<StateTransition> {
        std::mem::take(&mut self.pending_state_changes)
    }

    /// Queue `state` to be entered at `when`. Replaces any schedule already standing
    /// for that same state, so a re-swing re-times its own phases rather than
    /// stacking a second copy.
    pub fn schedule_state(&mut self, when: Instant, state: ActorStateType) {
        self.scheduled_states.retain(|(_, s)| *s != state);
        self.scheduled_states.push((when, state));
        self.scheduled_states.sort_by_key(|(t, _)| *t);
    }

    /// Drop every pending schedule — used when something overrides the swing the
    /// schedule belonged to (a stagger, a paralyse, death, a round reset). Without
    /// this, a swing's queued `Recovery`/`Idle` would fire *after* the interrupt and
    /// silently un-stagger the fighter.
    pub fn clear_scheduled_states(&mut self) {
        self.scheduled_states.clear();
    }

    /// Apply every scheduled transition now due, oldest first. Each one goes through
    /// [`Self::set_actor_state`], so it lands on the outbox like any other.
    pub fn reconcile_scheduled_states(&mut self, now: Instant) {
        while let Some(&(when, state)) = self.scheduled_states.first() {
            if when > now {
                break;
            }
            self.scheduled_states.remove(0);
            self.set_actor_state(state, now);
        }
    }

    /// Seconds spent in the current actor state — the `timeInState` float the
    /// `PlayerStateChange` family carries at propId 8.
    pub fn time_in_state(&self, now: Instant) -> f32 {
        now.saturating_duration_since(self.state_entered).as_secs_f32()
    }

    /// This fighter's live **Block Rating** while guarding (Phase 3.5): the summed
    /// weapon + shield `blockBase`, multiplied by `optimalBlockBoost` and the UESP
    /// high-block ×2 when the guard is in its OPTIMAL phase.
    pub fn block_rating(&self, optimal: bool) -> f32 {
        use super::tables;
        let base = self.loadout.block_rating;
        if !optimal {
            return base;
        }
        let boost = self
            .loadout
            .shield_optimal_block_boost
            .max(self.loadout.weapon_optimal_block_boost)
            .max(1.0);
        base * boost * tables::OPTIMAL_BLOCK_RATING_MULTIPLIER
    }

    /// True iff this fighter is currently staggered. [Phase 3.13]
    pub fn is_staggered(&self, now: Instant) -> bool {
        matches!(self.staggered_until, Some(t) if now < t)
    }

    /// Enter the staggered state for `CombatParameters.baseStaggerDuration`.
    pub fn apply_stagger(&mut self, now: Instant) {
        self.staggered_until =
            Some(now + std::time::Duration::from_secs_f32(BASE_STAGGER_DURATION_SECS));
        self.set_actor_state(ActorStateType::Staggered, now);
        // A stagger overrides whatever swing was in flight: drop its queued
        // follow-through/recovery/idle, which would otherwise fire mid-stagger and
        // return the actor to Idle early.
        self.clear_scheduled_states();
        // A staggered fighter's guard drops and its combo chain breaks.
        self.blocking_until = None;
        self.block_raised_at = None;
        self.reset_combo();
    }

    /// Clear an expired stagger, returning true when the fighter just recovered.
    pub fn reconcile_stagger(&mut self, now: Instant) -> bool {
        if let Some(t) = self.staggered_until {
            if now >= t {
                self.staggered_until = None;
                if self.actor_state == ActorStateType::Staggered {
                    self.set_actor_state(ActorStateType::Idle, now);
                }
                return true;
            }
        }
        false
    }

    /// True when health is below `CombatParameters.criticalHealthThreshold` (35 %).
    /// [Phase 3.13]
    pub fn is_critical_health(&self) -> bool {
        self.max_health > 0
            && (self.health as f32 / self.max_health as f32) * 100.0 < CRITICAL_HEALTH_THRESHOLD_PCT
    }

    /// Consume one of this round's consumable charges, or `false` when the
    /// `consumablesPerRound` budget is spent. [Phase 4.3]
    pub fn try_use_consumable(&mut self) -> bool {
        if self.consumables_used >= CONSUMABLES_PER_ROUND {
            return false;
        }
        self.consumables_used += 1;
        true
    }

    /// Reset the combo chain (`combo_count` → 0, `last_combo_side` → None) — on an
    /// optimal block, a `Middle` maneuver, a non-alternating/late swing, or round
    /// start. Mirrors the client's `AttackerStateData.ResetCombo`.
    pub fn reset_combo(&mut self) {
        self.combo_count = 0;
        self.last_combo_side = ActiveSide::None;
    }

    /// Register a landed normal Left/Right swing and return the resulting combo count
    /// (post-increment). An *alternating* side vs `last_combo_side` continues the chain
    /// (`combo_count += 1`); a repeat side (or a None side) RESETS it to 0. `Middle`
    /// (maneuver) and blocks do not call this — they `reset_combo`. Mirrors
    /// `AttackerStateData.IncrementCombo`.
    pub fn register_combo_swing(&mut self, side: ActiveSide) -> u32 {
        let alternates = matches!(
            (self.last_combo_side, side),
            (ActiveSide::Left, ActiveSide::Right) | (ActiveSide::Right, ActiveSide::Left)
        );
        if alternates {
            self.combo_count = self.combo_count.saturating_add(1);
        } else {
            // First swing of a chain, or a repeated side → start a fresh chain at 0.
            self.combo_count = 0;
        }
        self.last_combo_side = side;
        self.combo_count
    }

    /// True iff this fighter's guard is up at `now` (a `PlayerBlockingStateChange`
    /// within the still-open block window). Reconciles `actor_state`/`blocking_side`
    /// back to Idle/None when the window has lapsed (so a stale block can't reduce
    /// damage forever). Records `last_block_dropped_at` on expiry for the OPTIMAL
    /// recovery cooldown.
    pub fn reconcile_block(&mut self, now: Instant) -> bool {
        let up = matches!(self.blocking_until, Some(t) if now < t);
        if !up && self.actor_state == ActorStateType::Blocking {
            // Blocking → Idle. This transition is what LOWERS the shield on both
            // screens; before the outbox existed it happened silently.
            self.set_actor_state(ActorStateType::Idle, now);
            self.blocking_side = ActiveSide::None;
            self.blocking_until = None;
            self.block_raised_at = None;
            self.last_block_dropped_at = Some(now);
        }
        up
    }

    /// The current OPTIMAL/LATE block phase for `now`, given the dump.cs constants:
    /// - OPTIMAL iff the guard has been up for < `BLOCK_OPTIMAL_TIME_SECS` **and**
    ///   the last block was dropped more than `OPTIMAL_BLOCK_RECOVERY_SECS` ago (or
    ///   was never dropped — first block of the match is always OPTIMAL);
    /// - LATE otherwise (held too long, or re-raised inside the recovery window).
    ///
    /// Returns `None` when the guard is not up.
    pub fn block_phase(&self, now: Instant) -> Option<BlockPhase> {
        let raised = self.block_raised_at?;
        // Guard must still be up.
        if !matches!(self.blocking_until, Some(until) if now < until) {
            return None;
        }
        let held_secs = now.duration_since(raised).as_secs_f32();
        if held_secs >= BLOCK_OPTIMAL_TIME_SECS {
            return Some(BlockPhase::Late);
        }
        // Within the 2s optimal window: check recovery cooldown.
        let in_recovery = self.last_block_dropped_at
            .map(|t| now.duration_since(t).as_secs_f32() < OPTIMAL_BLOCK_RECOVERY_SECS)
            .unwrap_or(false);
        Some(if in_recovery { BlockPhase::Late } else { BlockPhase::Optimal })
    }

    /// Return the sum of transient Resist-Elements resistances for `ty` (non-expired
    /// only). Called by the damage pipeline to add to loadout resistances. [§4.3]
    pub fn transient_resistance_against(&self, ty: DamageType, now: Instant) -> f32 {
        self.transient_resistances
            .iter()
            .filter(|(t, _, exp)| *t == ty && now < *exp)
            .map(|(_, v, _)| *v)
            .sum()
    }

    /// Prune expired transient resistances.
    pub fn prune_transient_resistances(&mut self, now: Instant) {
        self.transient_resistances.retain(|(_, _, exp)| now < *exp);
    }

    pub fn is_dead(&self) -> bool {
        self.health == 0
    }

    /// Apply `amount` raw damage to health, clamped at 0, and bump the stats seq.
    pub fn take_damage(&mut self, amount: u32) {
        self.health = self.health.saturating_sub(amount);
        self.stats_seq = self.stats_seq.wrapping_add(1);
    }

    /// The packed-stats ULong for `ReceiveDamage` propId 4/5: each pool encoded as
    /// its 10-bit fraction of max (`STAT_MAX` = full), + the sequence id in the hi32.
    pub fn packed_stats(&self) -> u64 {
        PackedStats::pack(
            wire_fraction(self.health, self.max_health),
            wire_fraction(self.stamina, self.max_stamina),
            wire_fraction(self.magicka, self.max_magicka),
            self.stats_seq,
        )
    }

    // ----- Conditioning / resistance / negation (status-resistance-spec §2/§4/§5) -----

    /// The flat resistance this fighter applies against an incoming `ty` component:
    /// summed `resistances` of that type (permanent loadout resistances), with ELEMENTAL
    /// resistance scaled by the attacker's `(1 − elem_resist_piercing)`, MINUS any
    /// matching `weaknesses`. Returns a non-negative flat amount. [§2.1/§2.3]
    pub fn resistance_against(&self, ty: DamageType, attacker_elem_pierce: f32) -> f32 {
        self.resistance_rating_against(ty, attacker_elem_pierce, 0.0)
    }

    /// The **Resistance Rating** this fighter applies against an incoming `ty`
    /// component (Phase 3.4). Elemental resistance is first reduced by the attacker's
    /// Elemental-Resistance-Piercing — as a **rating subtraction**
    /// (`pierce_rating`, the shipped `ResistancePiercingElementalPropertyLogic` value)
    /// and then by the legacy fractional `pierce_frac` (the ability-side
    /// `_elementalResistancePiercing`, which is a percentage). Never negative.
    pub fn resistance_rating_against(
        &self,
        ty: DamageType,
        pierce_frac: f32,
        pierce_rating: f32,
    ) -> f32 {
        let mut rating: f32 = self
            .loadout
            .resistances
            .iter()
            .filter(|(t, _)| *t == ty)
            .map(|(_, v)| *v)
            .sum();
        if super::damage::is_elemental(ty) {
            rating = (rating - pierce_rating).max(0.0);
            rating *= (1.0 - pierce_frac).clamp(0.0, 1.0);
        }
        rating.max(0.0)
    }

    /// The **Weakness Rating** this fighter suffers for an incoming `ty` component —
    /// a flat damage INCREASE (`increasePerWeaknessRating`), capped at
    /// `maximumWeaknessEffect`. Kept separate from resistance: netting the two (as the
    /// old model did) hid the cap and made a weakness silently cancel a resist.
    pub fn weakness_rating_against(&self, ty: DamageType) -> f32 {
        self.loadout
            .weaknesses
            .iter()
            .filter(|(t, _)| *t == ty)
            .map(|(_, v)| *v)
            .sum()
    }

    /// Combined flat resistance including transient Resist-Elements buffs (timed via
    /// `now`). The damage pipeline calls this instead of `resistance_against` so that
    /// the Resist-Elements flat reduction is applied AFTER block in the same step.
    pub fn total_resistance_against(&self, ty: DamageType, attacker_elem_pierce: f32, now: Instant) -> f32 {
        let perm = self.resistance_against(ty, attacker_elem_pierce);
        let transient = self.transient_resistance_against(ty, now);
        perm + transient
    }

    /// The per-condition land threshold (absolute HP) for `condition`: the base
    /// `HEALTH_PERCENT_TO_CAUSE_STATUS × max_health`, RAISED by any matching
    /// `status_resist` ("Fortify Poisoned/…") bump. [§5.2 + §5.5]
    pub fn condition_threshold(&self, condition: StatusEffectType) -> f32 {
        let base = HEALTH_PERCENT_TO_CAUSE_STATUS * self.max_health as f32;
        let bump: f32 = self
            .loadout
            .status_resist
            .iter()
            .filter(|(c, _)| *c == condition)
            .map(|(_, frac)| *frac * self.max_health as f32)
            .sum();
        base + bump
    }

    /// The elemental amplification factor for an attacker's `ty` enchant track against
    /// THIS defender — driven by the matching element's accumulated conditioning in the
    /// window (`damage::element_amp`). Non-elemental types never amplify. [§4.3]
    pub fn element_amp_for(&self, ty: DamageType) -> f32 {
        let Some(condition) = condition_for_element(ty) else {
            return 1.0;
        };
        super::damage::element_amp(self.recent_element_damage(ty), self.condition_threshold(condition))
    }

    /// Sum of the (non-expired) accumulated damage of `ty` in the sliding window.
    pub fn recent_element_damage(&self, ty: DamageType) -> f32 {
        self.damage_history.get(&ty).map(|v| v.iter().map(|(a, _)| *a).sum()).unwrap_or(0.0)
    }

    /// Push a landed elemental component into the window + prune lapsed entries. Called
    /// for each elemental component AFTER block/resist/negate. [§5.5]
    pub fn record_element_damage(&mut self, ty: DamageType, amount: f32, now: Instant) {
        if amount <= 0.0 || !super::damage::is_elemental(ty) {
            return;
        }
        let entries = self.damage_history.entry(ty).or_default();
        entries.push((amount, now));
        entries.retain(|(_, t)| now.duration_since(*t) < DAMAGE_HISTORY_WINDOW);
    }

    /// Drain the active negation pools (Ward/Absorb/Dodge, in source order) against the
    /// per-type `components` IN PLACE; expired pools are dropped first. Returns
    /// `(negated, heal)`: `negated` = the WHOLE hit's health damage was eaten (→ emit
    /// op66, skip HP), `heal` = Absorb's restoration of what it negated. [§4.5/§4.6]
    ///
    /// NOTE: takes `now` via the pools' `expires_at` (the caller prunes by passing the
    /// current instant through [`Self::prune_negation_pools`] first).
    pub fn apply_negation_pools(&mut self, components: &mut [(DamageType, f32)]) -> NegationResult {
        if self.negation_pools.is_empty() {
            return NegationResult { negated: false, heal: 0.0 };
        }
        let health_before: f32 = components
            .iter()
            .filter(|(t, _)| super::damage::is_health_type(*t))
            .map(|(_, v)| *v)
            .sum();
        if health_before <= 0.0 {
            return NegationResult { negated: false, heal: 0.0 };
        }
        let mut heal = 0.0;
        for pool in self.negation_pools.iter_mut() {
            if pool.remaining <= 0.0 {
                continue;
            }
            // Drain this pool across the remaining health components (in order).
            for (ty, v) in components.iter_mut() {
                if !super::damage::is_health_type(*ty) || *v <= 0.0 || pool.remaining <= 0.0 {
                    continue;
                }
                let eaten = v.min(pool.remaining);
                *v -= eaten;
                pool.remaining -= eaten;
                heal += eaten * pool.restoration_factor;
            }
        }
        self.negation_pools.retain(|p| p.remaining > 0.0);
        let health_after: f32 = components
            .iter()
            .filter(|(t, _)| super::damage::is_health_type(*t))
            .map(|(_, v)| *v)
            .sum();
        NegationResult { negated: health_after <= 0.0, heal }
    }

    /// Drop negation pools whose duration has lapsed (call on tick / before a hit).
    pub fn prune_negation_pools(&mut self, now: Instant) {
        self.negation_pools.retain(|p| now < p.expires_at);
    }

    /// True iff this fighter is currently paralysed (its inputs are blocked).
    pub fn is_paralyzed(&self) -> bool {
        self.actor_state == ActorStateType::Paralyzed
    }
}

/// How a round ended. [Phase 3.14]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundOutcome {
    /// Both fighters still alive.
    Ongoing,
    /// Exactly one fighter died — `winner` scores the round.
    Win { winner: usize },
    /// **Both** fighters hit 0 HP in the same resolution step: no score, the round
    /// is replayed. [Phase 3.14 — AUTHORED, no capture evidence]
    DoubleKo,
}

/// Result of draining the negation pools against a hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NegationResult {
    pub negated: bool,
    pub heal: f32,
}

// ---------------------------------------------------------------------------
// Per-match authoritative state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MatchCombat {
    pub phase: FlowState,
    /// Number of FIGHTERS in the match (1 or 2). For a solo-vs-bot match this is 2
    /// (player + bot) even though only 1 real peer connects — see `expected_peers`.
    pub capacity: usize,
    /// Real ENet peers to wait for before the round starts (the
    /// Connecting→BackendMatchCreated gate). Equals `capacity` for PvP; for a
    /// solo-vs-bot match it's 1 (the bot has no peer) so the match starts on the
    /// lone player's connect instead of hanging in Connecting forever.
    pub expected_peers: usize,
    /// 1 or 2 fighters (created at allocation from each player's loadout).
    pub fighters: Vec<Fighter>,
    /// The Control net object that carries flow-control stateName messages
    /// (captures used 560/561 for the round/flow controller).
    pub flow_controller_id: i32,
    /// The single type-54 **Match** net object id. Its replicated propId5 is the
    /// `MatchState` the client reads to bind players + advance the match (s506 obj
    /// 123). Allocated by `MatchInstance::new`.
    pub match_net_object_id: i32,
    /// The current replicated `MatchState` on the Match net object (propId5). Starts
    /// `Idle`; the FSM drives it through `WaitingForPlayers`(3)→`InitialPlayerSetup`(4)
    /// →`BackendMatchCreation`(5) at round start (the player-binding gate).
    pub match_state: MatchState,
    /// The match's `gameSessionId` (Match net-object propId9). Set by `MatchInstance`
    /// from the registry; a nil UUID until then (the binding gate is propId5, not 9).
    pub game_session_id: String,
    /// Next Avatar net object id to hand out (captures used 564–566).
    pub next_net_object_id: i32,
    pub round: u8,
    pub rounds_won: [u8; 2],
    /// Ordered per-round outcome: the SLOT that won round 1, round 2, … .
    ///
    /// `rounds_won` is only a tally, but op48 carries the cumulative round-by-round
    /// result ARRAY and the client tallies the displayed score from it — so the
    /// ORDER and the count both go on the wire. Capture-pinned: every one of the 375
    /// captured op48 frames fills (5,6),(7,8),(9,10) for rounds 1..N and sets
    /// propId 11 = N-1. See `messages::match_post_round_info`.
    pub round_winners: Vec<usize>,
    /// When the current flow phase started (drives StateTimeout heartbeat /
    /// round timers from the tick).
    pub phase_entered: Instant,
    /// Slot of the fighter that WON the match (the survivor), set by `resolve` when a
    /// fighter reaches 0 HP and the match ends. Drives the op48 result + the
    /// post-match MatchState walk. `None` until the match ends.
    pub winner: Option<usize>,
    /// Cursor into [`engine::MATCH_STATE_MATCHEND_PROGRESSION`] while the FSM walks the
    /// terminal post-round states (`BackendMatchEnd`→`PostMatch`→`DisconnectingPlayers`)
    /// after a round-ending death. Starts at 0 when the match enters `RoundEnd`; the
    /// FSM advances it on per-state timers (s506 obj-123 final-round timing) until the
    /// terminal state is broadcast, then finishes the match. Reset per match.
    pub matchend_step: usize,
    /// Cursor into [`engine::MATCH_STATE_INTERROUND_PROGRESSION`] while the FSM walks the
    /// BETWEEN-ROUNDS states (`ChooseLoadout`(8)→…→`InRound`(13)) after a NON-final
    /// round-ending death (best-of-3, neither player at 2 wins yet). Starts at 0 when the
    /// match enters `NextState`; the FSM advances it on the s506 round-0→round-1 timers
    /// until `InRound`(13), then resets both fighters to full HP and re-enters the live
    /// round (`StateTimeout`). Reset at the start of each between-rounds walk.
    pub interround_step: usize,
    /// When the last stat-regen tick fired. Initialised to the match's `phase_entered`
    /// so the first tick fires 1s into the live round. [spec §2]
    pub last_regen_tick: std::time::Instant,
}

impl MatchCombat {
    pub fn new(capacity: usize, expected_peers: usize, now: Instant) -> Self {
        MatchCombat {
            phase: FlowState::Connecting,
            capacity,
            expected_peers,
            fighters: Vec::with_capacity(capacity),
            flow_controller_id: 560, // matches captured flow-controller id range
            match_net_object_id: 0,  // assigned by MatchInstance::new
            match_state: MatchState::Idle,
            game_session_id: String::new(), // set by MatchInstance::new from the registry
            next_net_object_id: 564, // matches captured combat-actor id range
            round: 0,
            rounds_won: [0; 2],
            round_winners: Vec::new(),
            phase_entered: now,
            winner: None,
            matchend_step: 0,
            interround_step: 0,
            last_regen_tick: now,
        }
    }

    /// True iff some fighter has reached the best-of-3 round-win target (2). When this
    /// holds at a round-ending death, that death ends the MATCH; otherwise the match
    /// loops to the next round. `MaxMatchRounds` is 3 (`messages::MATCH_MAX_ROUNDS`,
    /// s506 Match propId8) → first to `ROUND_WINS_TO_WIN_MATCH` wins.
    pub fn match_is_won(&self) -> bool {
        self.rounds_won.iter().any(|&w| w >= ROUND_WINS_TO_WIN_MATCH)
    }

    /// How a round ended. [Phase 3.14]
    pub fn round_outcome(&self) -> RoundOutcome {
        let dead: Vec<usize> = self
            .fighters
            .iter()
            .enumerate()
            .filter(|(_, f)| f.is_dead())
            .map(|(i, _)| i)
            .collect();
        match dead.len() {
            0 => RoundOutcome::Ongoing,
            1 => RoundOutcome::Win {
                winner: self.opponent_of(dead[0]).unwrap_or(dead[0]),
            },
            // Simultaneous 0 HP: nobody scores, the round is replayed.
            _ => RoundOutcome::DoubleKo,
        }
    }

    /// The match winner when the final round ends 1-1 (no fighter reached 2 wins) —
    /// **Phase 3.14, AUTHORED, not capture-derived.** No recorded match ended in a
    /// draw, so the tiebreak below is a designed rule, not a reproduction:
    ///
    /// 1. higher remaining HP **fraction** wins;
    /// 2. if the fractions tie, the LOWER `pvpTrophies` wins (the underdog);
    /// 3. if those tie too, slot 0.
    ///
    /// `trophies` is `(slot0, slot1)` from the players' `pvpTrophies`.
    pub fn draw_tiebreak_winner(&self, trophies: (i64, i64)) -> usize {
        if self.fighters.len() < 2 {
            return 0;
        }
        let frac = |f: &Fighter| {
            if f.max_health == 0 {
                0.0
            } else {
                f.health as f32 / f.max_health as f32
            }
        };
        let (a, b) = (frac(&self.fighters[0]), frac(&self.fighters[1]));
        if (a - b).abs() > 1e-4 {
            return if a > b { 0 } else { 1 };
        }
        if trophies.0 != trophies.1 {
            return if trophies.0 < trophies.1 { 0 } else { 1 };
        }
        0
    }

    /// Reset both fighters to full pools for the next round (best-of-3 loop): HP/
    /// Stamina/Magicka back to max, clear cooldowns / status effects / block /
    /// swing-throttle, actor back to Idle. The stats sequence id keeps rising
    /// (monotonic across the whole match, as the wire expects). `round` is NOT
    /// touched here — the engine bumps it when the next round goes live.
    pub fn reset_fighters_for_next_round(&mut self, now: Instant) {
        for f in &mut self.fighters {
            f.health = f.max_health;
            f.stamina = f.max_stamina;
            f.magicka = f.max_magicka;
            f.stats_seq = f.stats_seq.wrapping_add(1);
            f.cooldowns.clear();
            f.effects.clear();
            // Between rounds the client tears the combat scene down and rebuilds it,
            // so a queued transition from the round that just ended would arrive
            // against a stale avatar. Reset the state silently and drop the outbox.
            f.actor_state = ActorStateType::Idle;
            f.pending_state_changes.clear();
            f.scheduled_states.clear();
            // The history ring and its index are per-ROUND (retail's firstIndex
            // restarts at 0 each round), so they reset with everything else.
            f.state_history.clear();
            f.transitions_total = 0;
            f.state_entered = now;
            f.blocking_side = ActiveSide::None;
            f.blocking_until = None;
            f.block_raised_at = None;
            f.last_block_dropped_at = None;
            f.last_swing = None;
            f.charge_press_at = None;
            f.charge_side = None;
            f.bot_swing_at = None;
            f.reset_combo();
            // Phase 4.1: drop last round's pointer geometry so the first swing of the
            // new round can never be classified from a stale pre-reset sample.
            f.last_input_x = None;
            f.last_input_y = None;
            f.last_input_at = None;
            f.last_client_charge = None;
            f.last_input_block_zone = None;
            f.damage_history.clear(); // ClearDamageHistory on round reset (§5.5)
            f.negation_pools.clear();
            f.transient_resistances.clear();
            f.staggered_until = None;
            f.consumables_used = 0; // consumablesPerRound is PER ROUND [Phase 4.3]
        }
        // Anchor the regen timer to now so the next round's first tick fires 1s in.
        self.last_regen_tick = now;
    }

    pub fn alloc_net_object_id(&mut self) -> i32 {
        let id = self.next_net_object_id;
        self.next_net_object_id += 1;
        id
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Real ENet peers to wait for before starting (see the field). A solo-vs-bot
    /// match has `capacity` 2 but `expected_peers` 1.
    pub fn expected_peers(&self) -> usize {
        self.expected_peers
    }

    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            FlowState::Connecting => "Connecting",
            FlowState::Spawning => "Spawning",
            FlowState::BackendMatchCreated => "BackendMatchCreated",
            FlowState::StateTimeout => "StateTimeout",
            FlowState::NextState => "NextState",
            FlowState::RoundEnd => "RoundEnd",
            FlowState::Finished => "Finished",
        }
    }

    /// Slot of the opponent of `slot` in a 2-player match (0↔1).
    pub fn opponent_of(&self, slot: usize) -> Option<usize> {
        if self.capacity < 2 {
            return None;
        }
        Some(1 - slot.min(1))
    }

    /// `(winner_char_uuid, loser_char_uuid)` for the match-end op48/op49 header, from the
    /// `winner` slot set at the match-ending death. Falls back to empty strings if the
    /// winner isn't set or a fighter is missing. The loser is the winner's opponent.
    pub fn winner_loser_uuids(&self) -> (String, String) {
        let Some(winner) = self.winner else {
            return (String::new(), String::new());
        };
        let loser = self.opponent_of(winner).unwrap_or(winner);
        let uuid = |slot: usize| {
            self.fighters.get(slot).map(|f| f.loadout.character_uuid.clone()).unwrap_or_default()
        };
        (uuid(winner), uuid(loser))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_stats_exact() {
        // Round-trips and bit-packing match the ReceiveDamage layout.
        let v = PackedStats::pack(812, 640, 300, 627_048_447);
        assert_eq!(PackedStats::unpack(v), (812, 640, 300, 627_048_447));
        // Clamps to STAT_MAX.
        let c = PackedStats::pack(5000, 5000, 5000, 0);
        assert_eq!(PackedStats::unpack(c), (STAT_MAX, STAT_MAX, STAT_MAX, 0));
    }

    #[test]
    fn fighter_target_is_opponent() {
        let now = Instant::now();
        let a = Fighter::new(0, 564, Loadout::default(), now);
        let b = Fighter::new(1, 565, Loadout::default(), now);
        assert_eq!(a.arena_target, 1);
        assert_eq!(b.arena_target, 0);
    }

    #[test]
    fn take_damage_clamps_and_bumps_seq() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 564, Loadout::default(), now);
        f.health = 30;
        f.take_damage(50);
        assert_eq!(f.health, 0);
        assert!(f.is_dead());
        assert_eq!(f.stats_seq, 1);
    }

    #[test]
    fn arena_triples_health_and_wire_is_fraction() {
        let now = Instant::now();
        // Level-30 fighter: base 200 + 290 = 490, ×3 arena = 1470 raw HP.
        let f0 = Fighter::new(0, 564, Loadout { level: 30, ..Default::default() }, now);
        assert_eq!(f0.max_health, health_for_level(30) * ARENA_HEALTH_MULTIPLIER);
        assert_eq!(f0.max_health, 1470);
        // Full pool → wire health fraction == STAT_MAX (full bar).
        let (h_full, _, _, _) = PackedStats::unpack(f0.packed_stats());
        assert_eq!(h_full, STAT_MAX);
        // Half raw HP → ~half the wire fraction (proves the wire packs a FRACTION
        // of max, not raw HP — 1470 wouldn't fit the 10-bit field).
        let mut f = f0;
        f.health = f.max_health / 2;
        let (h_half, _, _, _) = PackedStats::unpack(f.packed_stats());
        assert!((h_half as i32 - STAT_MAX as i32 / 2).abs() <= 1, "half HP → ~half wire, got {h_half}");
    }

    #[test]
    fn flow_wire_names() {
        assert_eq!(FlowState::BackendMatchCreated.wire_name(), Some("BackendMatchCreated"));
        assert_eq!(FlowState::StateTimeout.wire_name(), Some("StateTimeout"));
        assert_eq!(FlowState::Connecting.wire_name(), None);
    }

    /// COMBO state (§4.2): alternating Left/Right swings ramp `combo_count`; a repeat
    /// side or a `reset_combo` (block / round / maneuver) restarts the chain at 0.
    #[test]
    fn combo_counter_ramps_on_alternating_resets_on_repeat() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 564, Loadout::default(), now);
        assert_eq!(f.register_combo_swing(ActiveSide::Right), 0, "first swing = combo 0");
        assert_eq!(f.register_combo_swing(ActiveSide::Left), 1, "alternating → combo 1");
        assert_eq!(f.register_combo_swing(ActiveSide::Right), 2, "alternating → combo 2");
        // A repeated side restarts the chain.
        assert_eq!(f.register_combo_swing(ActiveSide::Right), 0, "repeat side → chain restarts at 0");
        // An explicit reset (optimal block / round) zeroes it.
        f.register_combo_swing(ActiveSide::Left);
        f.reset_combo();
        assert_eq!(f.combo_count, 0);
        assert_eq!(f.last_combo_side, ActiveSide::None);
    }

    /// CONDITIONING window (§5): elemental damage accumulates in the sliding window and
    /// drives the condition threshold; Fortify-<Condition> raises the threshold.
    #[test]
    fn conditioning_window_accumulates_and_threshold_scales() {
        let now = Instant::now();
        let mut f = Fighter::new(1, 565, Loadout { level: 100, ..Default::default() }, now);
        let max = f.max_health as f32;
        assert_eq!(f.recent_element_damage(DamageType::Poison), 0.0, "empty window");
        f.record_element_damage(DamageType::Poison, 100.0, now);
        f.record_element_damage(DamageType::Poison, 50.0, now);
        assert_eq!(f.recent_element_damage(DamageType::Poison), 150.0, "window sums recent poison");
        // Non-elemental + zero are ignored.
        f.record_element_damage(DamageType::Slashing, 999.0, now);
        f.record_element_damage(DamageType::Poison, 0.0, now);
        assert_eq!(f.recent_element_damage(DamageType::Slashing), 0.0, "physical is not conditioned");

        // Base Poisoned threshold = 25% of max HP; Fortify-Poisoned raises it.
        let base = f.condition_threshold(StatusEffectType::Poisoned);
        assert!((base - HEALTH_PERCENT_TO_CAUSE_STATUS * max).abs() < 1e-2, "base threshold = 25% max HP");
        f.loadout.status_resist = vec![(StatusEffectType::Poisoned, 0.10)];
        let bumped = f.condition_threshold(StatusEffectType::Poisoned);
        assert!(bumped > base, "Fortify Poisoned raises the threshold");
        assert!((bumped - (HEALTH_PERCENT_TO_CAUSE_STATUS + 0.10) * max).abs() < 1e-2, "+10% of max HP bump");
    }

    /// RESISTANCE (§2): flat per-type subtraction, with elemental resist scaled by the
    /// attacker's Elemental-Resistance-Piercing; weakness reduces effective resist.
    #[test]
    fn resistance_against_flat_with_piercing() {
        let now = Instant::now();
        let mut f = Fighter::new(1, 565, Loadout { level: 100, ..Default::default() }, now);
        f.loadout.resistances = vec![(DamageType::Poison, 40.0), (DamageType::Slashing, 20.0)];
        // No piercing: full flat resist.
        assert_eq!(f.resistance_against(DamageType::Poison, 0.0), 40.0);
        assert_eq!(f.resistance_against(DamageType::Slashing, 0.0), 20.0, "piercing doesn't touch physical");
        // 50% elem piercing halves the ELEMENTAL resist only.
        assert_eq!(f.resistance_against(DamageType::Poison, 0.5), 20.0);
        assert_eq!(f.resistance_against(DamageType::Slashing, 0.5), 20.0, "physical resist unaffected by elem piercing");
        // Phase 3.4: WEAKNESS is now a SEPARATE flat increase (`increasePerWeaknessRating`,
        // capped by `maximumWeaknessEffect`) rather than being netted off the resistance —
        // netting them hid the cap and let a weakness silently cancel a resist.
        f.loadout.weaknesses = vec![(DamageType::Poison, 50.0)];
        assert_eq!(f.resistance_against(DamageType::Poison, 0.0), 40.0, "resistance is untouched by weakness");
        assert_eq!(f.weakness_rating_against(DamageType::Poison), 50.0);
        assert_eq!(f.weakness_rating_against(DamageType::Slashing), 0.0);
        // Elemental-Resistance-PIERCING can also be a RATING subtraction (Phase 3.4).
        assert_eq!(f.resistance_rating_against(DamageType::Poison, 0.0, 15.0), 25.0);
        assert_eq!(f.resistance_rating_against(DamageType::Poison, 0.0, 999.0), 0.0, "never negative");
    }

    // -----------------------------------------------------------------------
    // Mechanic 1: BLOCK OPTIMAL→LATE timeout [§Mechanic-1]
    // -----------------------------------------------------------------------

    /// Block phase is OPTIMAL when the guard was just raised (within 2.0s window, no
    /// recovery cooldown). After 2.0s of continuous holding it degrades to LATE.
    /// [PvpDefaultSettings BLOCK_OPTIMAL_TIME=2.0 / OPTIMAL_BLOCK_RECOVERY_TIME=0.8]
    #[test]
    fn block_degrades_from_optimal_to_late_after_2s() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 564, Loadout { level: 50, ..Default::default() }, now);
        let block_window = std::time::Duration::from_secs(5); // long window so it doesn't expire

        // Fresh block: raised just now → OPTIMAL.
        f.set_actor_state(ActorStateType::Blocking, now);
        f.blocking_side = ActiveSide::Right;
        f.blocking_until = Some(now + block_window);
        f.block_raised_at = Some(now);
        assert_eq!(
            f.block_phase(now),
            Some(BlockPhase::Optimal),
            "freshly raised block is OPTIMAL"
        );

        // Still within 2.0s window → OPTIMAL.
        let within = now + std::time::Duration::from_millis(1500);
        assert_eq!(
            f.block_phase(within),
            Some(BlockPhase::Optimal),
            "1.5s hold is still OPTIMAL (< 2.0s)"
        );

        // After 2.0s → LATE.
        let after = now + std::time::Duration::from_millis(2001);
        assert_eq!(
            f.block_phase(after),
            Some(BlockPhase::Late),
            "2.0s+ hold degrades to LATE"
        );
    }

    /// A block re-raised within the OPTIMAL_BLOCK_RECOVERY_TIME (0.8s) window starts as
    /// LATE (not OPTIMAL) — the recovery cooldown prevents rapid OPTIMAL chaining.
    #[test]
    fn block_reraise_within_recovery_window_is_late() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 564, Loadout { level: 50, ..Default::default() }, now);
        let block_window = std::time::Duration::from_secs(5);

        // Drop the block (record last_block_dropped_at = now).
        f.last_block_dropped_at = Some(now);

        // Re-raise inside the `postOptimalBlockResetTime` (1.4 s) recovery window.
        let reraise = now + std::time::Duration::from_millis(300);
        f.set_actor_state(ActorStateType::Blocking, reraise);
        f.blocking_side = ActiveSide::Right;
        f.blocking_until = Some(reraise + block_window);
        f.block_raised_at = Some(reraise);

        assert_eq!(
            f.block_phase(reraise),
            Some(BlockPhase::Late),
            "re-raised within postOptimalBlockResetTime (1.4 s) → starts as LATE, not OPTIMAL"
        );

        // Phase 3.5: the recovery window is `postOptimalBlockResetTime` = 1.4 s
        // (PlayerCombatParameters), NOT the dump's 0.8 s server-cheat default — so a
        // raise at 0.9 s is still LATE and only a raise past 1.4 s is OPTIMAL again.
        assert!(
            (OPTIMAL_BLOCK_RECOVERY_SECS - 1.4).abs() < 1e-6,
            "postOptimalBlockResetTime is 1.4 s, got {OPTIMAL_BLOCK_RECOVERY_SECS}"
        );
        let still_late = now + std::time::Duration::from_millis(900);
        f.block_raised_at = Some(still_late);
        f.blocking_until = Some(still_late + block_window);
        assert_eq!(f.block_phase(still_late), Some(BlockPhase::Late), "0.9 s < 1.4 s → still LATE");

        let after_recovery = now + std::time::Duration::from_millis(1500);
        f.block_raised_at = Some(after_recovery);
        f.blocking_until = Some(after_recovery + block_window);
        assert_eq!(
            f.block_phase(after_recovery),
            Some(BlockPhase::Optimal),
            "re-raised after the 1.4 s reset → OPTIMAL"
        );
    }

    // -----------------------------------------------------------------------
    // Mechanic 3: RESIST ELEMENTS transient resistance [§Mechanic-3]
    // -----------------------------------------------------------------------

    /// Resist-Elements adds a transient flat resistance for all four elemental types;
    /// it is included in `total_resistance_against` and expires after 11.5s.
    #[test]
    fn resist_elements_flat_subtraction_after_block_via_transient() {
        let now = Instant::now();
        let mut f = Fighter::new(1, 565, Loadout { level: 50, ..Default::default() }, now);
        let expires = now + std::time::Duration::from_secs(12);

        // Push Resist-Elements transient resistances for all four element types (50 each).
        for ty in [DamageType::Fire, DamageType::Frost, DamageType::Shock, DamageType::Poison] {
            f.transient_resistances.push((ty, 50.0, expires));
        }

        // Each elemental type has 50 flat resist NOW; expires AFTER now.
        assert!((f.total_resistance_against(DamageType::Poison, 0.0, now) - 50.0).abs() < 1e-3,
            "transient Poison resist = 50");
        assert!((f.total_resistance_against(DamageType::Fire, 0.0, now) - 50.0).abs() < 1e-3,
            "transient Fire resist = 50");
        // Physical is NOT covered by Resist-Elements (only elemental four).
        assert_eq!(f.total_resistance_against(DamageType::Slashing, 0.0, now), 0.0,
            "Slashing has no transient resist from Resist-Elements");

        // After expiry the transient resist disappears.
        let after = now + std::time::Duration::from_secs(13);
        assert_eq!(f.total_resistance_against(DamageType::Poison, 0.0, after), 0.0,
            "transient resist expires after its duration");
    }

    // -----------------------------------------------------------------------
    // Mechanic 4: DoT concurrent stacking [§Mechanic-4]
    // -----------------------------------------------------------------------

    /// Multiple concurrent DoT ActiveEffect instances on the same fighter tick
    /// INDEPENDENTLY — their `last_tick` and `per_tick_damage` are independent.
    #[test]
    fn dot_concurrent_instances_stack_independently() {
        let now = Instant::now();
        let expires = now + std::time::Duration::from_secs(5);
        let mut f = Fighter::new(1, 565, Loadout { level: 50, ..Default::default() }, now);

        // Push two concurrent Poisoned effects with different per-tick magnitudes
        // (mimics s506: Flappety had 1.25/tick + 4.42/tick concurrently).
        f.effects.push(ActiveEffect {
            effect: StatusEffectType::Poisoned,
            damage_type: DamageType::Poison,
            value: 1.25,
            per_tick_damage: 1.25,
            expires_at: expires,
            last_tick: now,
            is_transient_resist: false,
        });
        f.effects.push(ActiveEffect {
            effect: StatusEffectType::Poisoned,
            damage_type: DamageType::Poison,
            value: 4.42,
            per_tick_damage: 4.42,
            expires_at: expires,
            last_tick: now,
            is_transient_resist: false,
        });

        assert_eq!(f.effects.len(), 2, "two independent DoT instances");
        let total_per_tick: f32 = f.effects.iter().map(|e| e.per_tick_damage).sum();
        assert!((total_per_tick - 5.67).abs() < 1e-3,
            "combined tick = 1.25 + 4.42 = 5.67 (concurrent, not refreshed/merged)");

        // Verify they expire at the same time (both created simultaneously).
        assert!(f.effects.iter().all(|e| e.expires_at == expires),
            "both instances share the same expiry");
    }
}
