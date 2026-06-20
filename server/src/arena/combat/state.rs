//! Authoritative per-match combat state + the arena protocol enums.
//!
//! Enum discriminants are the on-wire values from `reference/il2cpp/dump.cs` /
//! `reference/il2cpp/arena-opcodes.json` and the field-level decode in the
//! capture repo's `docs/archive/arena-combat-reference.md`. Where an enum is only
//! partially mapped it is marked `// …` — extend as more values are confirmed.

use std::collections::HashMap;
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
/// (`PlayerChannelingStateChange` stateId etc.). Partially mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ActorStateType {
    Idle = 0,
    /// Blocking. NOTE: the real wire `stateId` is TBD (not yet confirmed from
    /// dump.cs); this discriminant is used **server-internally only** (block
    /// resolution) and must not collide with the confirmed values below. Fix the
    /// value before serializing a blocking state-change on the wire.
    Blocking = 1,
    Channeling = 4,
    Staggered = 5,
    Dialogue = 8,
    /// `ActorParalyzedState` — the paralysed actor state. **StateId 13**
    /// (`dump.cs` 340018/340188; `arena-status-resistance-spec.md` §5.4). The victim's
    /// inputs are blocked for the `Paralyzed` status duration (3.1 s). Was previously a
    /// placeholder; fixed to the dump's real StateId.
    Paralyzed = 13,
    PlayerAutoAttack = 19,
    Emote = 28,
    // … (Recovery / FollowThrough / Charging / Draining / Maneuver discriminants
    //    TBD from dump.cs when those state-change messages are built).
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

/// `OPTIMAL_BLOCK_RECOVERY_TIME` (dump.cs 427015): cooldown (seconds) after dropping
/// the block before a new OPTIMAL window can begin. Re-raising within this window
/// starts as LATE, not OPTIMAL.
pub const OPTIMAL_BLOCK_RECOVERY_SECS: f32 = 0.8;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AbilityTag {
    #[default]
    /// Generic damage spell — handled by the default `resolve_ability` path.
    Generic,
    /// `WardAbility` subclass: apply Ward negation pool + armor. [§Mechanic-4]
    Ward,
    /// `ResistElementsAbility`: apply 4-tuple elemental resistance. [§Mechanic-3]
    ResistElements,
}

/// One equipped ability: its instance UUID (as referenced by
/// `RequestExecuteAbility`) and its level (drives scaling/cooldown).
#[derive(Debug, Clone)]
pub struct EquippedAbility {
    pub instance_uuid: String,
    pub level: u8,
    pub tag: AbilityTag,
}

/// The weapon's base damage profile (per-type), filled from game data / RE.
#[derive(Debug, Clone, Default)]
pub struct WeaponProfile {
    pub primary_type: Option<DamageType>,
    /// Base damage per type before swing/ability/enchant factors.
    pub base_by_type: Vec<(DamageType, f32)>,
    /// Weapon weight class — drives the combo/crit ramp (`damage::combo_factor`):
    /// a **Light** weapon (dagger) combos fast on chained alternating side-swings; a
    /// **Heavy** weapon leans on charged `Middle` crits. Recovered for the recorded
    /// match (s506) as **Light** (Dragonbone Dagger); `None` ⇒ the model's default
    /// (Light — the calibration target's class) until per-weapon item game-data is
    /// wired. See `loadout::from_character` (the fork lacks an item→weight table).
    pub weight: Option<crate::arena::combat::tables::Weight>,
}

/// A fighter's combat-relevant equipment, derived from the imported character.
#[derive(Debug, Clone, Default)]
pub struct Loadout {
    /// Character level — drives max-Health/Stamina/Magicka (`health_for_level`).
    pub level: u16,
    pub abilities: Vec<EquippedAbility>,
    pub weapon: WeaponProfile,
    pub has_shield: bool,
    /// Enchant `(damage_type, tier)` contributions, applied in the damage model.
    pub enchants: Vec<(DamageType, u8)>,
    /// Display name + character UUID for the round-start op50 spawn. Empty for the
    /// starter loadout (no character row); set by `loadout::from_character` + the
    /// matchmaker's character load.
    pub display_name: String,
    pub character_uuid: String,
    /// The two op54 round-start PROFILE JSON blobs (gear + full character), serialized
    /// from the stored character by the matchmaker; empty for the starter loadout.
    pub profile_equipped_json: String,
    pub profile_character_json: String,

    // --- Defensive / offensive enchant-derived fields (status-resistance-spec §2.5) ---
    /// Summed FLAT resistance per `DamageType` (armor "Resist X" enchants + perks).
    /// Applied as `afterResist = max(0, afterBlock − resist(t)·(1−elemPierce) + weakness)`.
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
    pub actor_state: ActorStateType,
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
/// (in the window) that LANDS the elemental condition. ≈0.25 for the elemental four
/// (`<game-data>`; representative). Arena triples max HP, so ~3× raw damage is needed.
/// [`arena-status-resistance-spec.md` §5.2 — calibration knob]
pub const HEALTH_PERCENT_TO_CAUSE_STATUS: f32 = 0.25;

/// `_damageToCauseParalyze` — the ABSOLUTE accumulated-poison threshold (in the window)
/// that lands `Paralyzed` (layered ON TOP of the Poisoned threshold). `<game-data>`:
/// set as a fraction of max HP so ~2-3 Paralyze casts land it (calibrated to the s506
/// cadence: a Paralyze-spell poison hit ≈137 + amp, ~0.45× max HP lands paralyse after
/// the poison condition). [§5.4 — calibration GUESS]
pub const PARALYZE_POISON_THRESHOLD_FRACTION: f32 = 0.45;

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
            actor_state: ActorStateType::Idle,
            state_entered: now,
            arena_target: 1 - slot.min(1), // 2-player: the other slot
            blocking_side: ActiveSide::None,
            blocking_until: None,
            block_raised_at: None,
            last_block_dropped_at: None,
            last_swing: None,
            combo_count: 0,
            last_combo_side: ActiveSide::None,
            damage_history: HashMap::new(),
            can_be_paralyzed: true, // players can be paralysed (vs boss innate immunity)
            negation_pools: Vec::new(),
            transient_resistances: Vec::new(),
        }
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
            self.actor_state = ActorStateType::Idle;
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
        let mut resist: f32 = self
            .loadout
            .resistances
            .iter()
            .filter(|(t, _)| *t == ty)
            .map(|(_, v)| *v)
            .sum();
        if super::damage::is_elemental(ty) {
            resist *= (1.0 - attacker_elem_pierce).clamp(0.0, 1.0);
        }
        let weakness: f32 = self
            .loadout
            .weaknesses
            .iter()
            .filter(|(t, _)| *t == ty)
            .map(|(_, v)| *v)
            .sum();
        (resist - weakness).max(0.0)
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
            f.actor_state = ActorStateType::Idle;
            f.state_entered = now;
            f.blocking_side = ActiveSide::None;
            f.blocking_until = None;
            f.block_raised_at = None;
            f.last_block_dropped_at = None;
            f.last_swing = None;
            f.reset_combo();
            f.damage_history.clear(); // ClearDamageHistory on round reset (§5.5)
            f.negation_pools.clear();
            f.transient_resistances.clear();
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
        // A weakness reduces effective resist (floored at 0).
        f.loadout.weaknesses = vec![(DamageType::Poison, 50.0)];
        assert_eq!(f.resistance_against(DamageType::Poison, 0.0), 0.0, "weakness > resist → 0 (more gets through)");
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
        f.actor_state = ActorStateType::Blocking;
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

        // Re-raise immediately (0.3s after drop — inside the 0.8s recovery window).
        let reraise = now + std::time::Duration::from_millis(300);
        f.actor_state = ActorStateType::Blocking;
        f.blocking_side = ActiveSide::Right;
        f.blocking_until = Some(reraise + block_window);
        f.block_raised_at = Some(reraise);

        assert_eq!(
            f.block_phase(reraise),
            Some(BlockPhase::Late),
            "re-raised within 0.8s recovery → starts as LATE, not OPTIMAL"
        );

        // After the recovery cooldown passes (>0.8s), a fresh raise is OPTIMAL again.
        let after_recovery = now + std::time::Duration::from_millis(900);
        f.block_raised_at = Some(after_recovery);
        f.blocking_until = Some(after_recovery + block_window);
        assert_eq!(
            f.block_phase(after_recovery),
            Some(BlockPhase::Optimal),
            "re-raised after 0.8s recovery → OPTIMAL"
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
