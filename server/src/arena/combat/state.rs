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

/// How many double-KO rounds a match may REPLAY. **AUTHORED, not capture-derived**
/// (no recorded match has a double KO). After one replay, the next double KO is
/// decided by [`MatchCombat::draw_tiebreak_winner`] like a timed-out round. Uncapped,
/// two phase-locked fighters whose killing blow and Revenge retaliation land in the
/// same step replay the identical round forever (CRE-SOAK seed 0x4d4e3b4e79f4ca03).
pub const MAX_DOUBLE_KO_REPLAYS: u8 = 1;

/// Hard backstop on rounds played in one match: best-of-3 plus the replays above.
/// With the replay cap a match cannot legitimately reach it undecided; if it ever
/// does, the match ends on the round tally (see `resolve::on_round_ended`).
pub const MATCH_ROUND_HARD_CAP: usize =
    (2 * ROUND_WINS_TO_WIN_MATCH as usize - 1) + MAX_DOUBLE_KO_REPLAYS as usize;

/// Base max-Health from the shipped `PlayerStatsData._playerStats._healthBase`.
pub const HEALTH_BASE: u32 = 200;
/// Health gained by each `_healthPoints` entry in the shipped per-level table.
pub const HEALTH_PER_LEVEL: u32 = 10;
/// `_perLevelData` awards one health point per level through level 50, then zero.
pub const MAX_HEALTH_LEVEL_INCREASES: u16 = 49;

/// Retail base max-Health for a level, before the arena's ×3 health multiplier.
///
/// `PlayerStatsData._perLevelData` has 99 rows and awards `_healthPoints: 1` on
/// the first 49 only. Accumulated health points are therefore
/// `min(level - 1, 49)`: 200 at level 1, 690 at level 50, and still 690 at
/// levels 89 and 100. The previous approximation kept adding 10 forever and
/// gave a level-89 fighter 1,080 base HP — 57% above the shipped ceiling.
pub fn health_for_level(level: u16) -> u32 {
    HEALTH_BASE
        + HEALTH_PER_LEVEL
            * u32::from(level.saturating_sub(1).min(MAX_HEALTH_LEVEL_INCREASES))
}

#[cfg(test)]
mod retail_health_curve {
    use super::*;

    #[test]
    fn the_shipped_curve_caps_at_level_50() {
        assert_eq!(
            health_for_level(0),
            HEALTH_BASE,
            "invalid zero saturates safely"
        );
        assert_eq!(health_for_level(1), 200, "level 1 is the base");
        assert_eq!(health_for_level(49), 680, "the last level before the cap");
        assert_eq!(health_for_level(50), 690, "all 49 health increases applied");
        for level in [51, 89, 100, u16::MAX] {
            assert_eq!(
                health_for_level(level),
                690,
                "level {level} stays capped"
            );
        }
    }

    #[test]
    fn fighter_constructor_uses_the_capped_curve() {
        let fighter = Fighter::new(
            0,
            1,
            Loadout { level: 89, ..Default::default() },
            Instant::now(),
        );
        assert_eq!(fighter.max_health, 690 * ARENA_HEALTH_MULTIPLIER);
        assert_eq!(
            fighter.health, fighter.max_health,
            "a fighter starts full"
        );
    }
}

/// Base pool before any attribute point is spent — `PlayerStatsData._playerStats`
/// `_staminaBase` / `_magickaBase` / `_healthBase`, all 200.0.
pub const POOL_BASE: u32 = 200;
/// What one spent attribute point adds — `_staminaIncrement` / `_magickaIncrement`,
/// both 10.0.
pub const POOL_PER_POINT: u32 = 10;
/// Attribute points stop at level 50: `_perLevelData` awards `_attributePoints: 1`
/// per level and then zero, so the accumulated total is `min(level - 1, 49)`.
pub const MAX_ATTRIBUTE_POINTS: u16 = 49;

/// A character's max Stamina or Magicka, from the points they actually spent.
///
/// EXTRACTED, then identity-checked against live characters. `PlayerStatsData` in the
/// APK bundle `duplicates_sab` gives `_staminaBase 200.0`, `_staminaIncrement 10.0`,
/// `_magickaBase 200.0`, `_magickaIncrement 10.0`, and a 99-row `_perLevelData` whose
/// accumulated `_attributePoints` equals `min(level - 1, 49)` on every one of the 99
/// rows.
///
/// The check that makes it more than a plausible reading: live characters agree
/// exactly. A level-40 has 24 + 15 = 39 points, a level-48 has 24 + 23 = 47, and the
/// level-89 from report #109 has 46 + 3 = 49 — `min(level-1, 49)` in all three.
///
/// This replaces [`pool_for_level`], which gave BOTH pools `200 + 5 * (level - 1)` and
/// ignored the split entirely: that level-89 was handed 640 stamina and 640 magicka
/// where retail gives him **660 and 230**. His Ward costs 205 magicka, so retail lets
/// him cast it once; we let him cast it three times.
pub fn pool_for_points(points: u16) -> u32 {
    POOL_BASE + POOL_PER_POINT * u32::from(points.min(MAX_ATTRIBUTE_POINTS))
}

#[cfg(test)]
mod report109_real_pools {
    use super::*;

    /// The extracted constants, pinned against the APK.
    ///
    /// `PlayerStatsData` (APK bundle `duplicates_sab`) `._playerStats`:
    /// `_staminaBase 200.0`, `_staminaIncrement 10.0`, `_magickaBase 200.0`,
    /// `_magickaIncrement 10.0`. Its 99-row `_perLevelData` awards
    /// `_attributePoints: 1` per level and then 0, reaching 49 at level 50.
    #[test]
    fn the_shipped_constants() {
        assert_eq!(POOL_BASE, 200);
        assert_eq!(POOL_PER_POINT, 10);
        assert_eq!(MAX_ATTRIBUTE_POINTS, 49);
        assert_eq!(pool_for_points(0), 200, "unspent is the base");
        assert_eq!(pool_for_points(49), 690, "the ceiling retail can reach");
        assert_eq!(pool_for_points(255), 690, "and it is a ceiling, not a ramp");
    }

    /// THE identity check, and the reason this is not a plausible reading of a
    /// data file: three live characters agree exactly with `min(level - 1, 49)`.
    ///
    ///   level 40 -> 24 stamina + 15 magicka = 39
    ///   level 48 -> 24 + 23             = 47
    ///   level 89 -> 46 + 3              = 49   (the report #109 reporter)
    #[test]
    fn live_characters_match_the_accumulation_model() {
        for (level, spent) in [(40u16, 39u16), (48, 47), (89, 49)] {
            assert_eq!(
                u32::from(spent.min(MAX_ATTRIBUTE_POINTS)),
                u32::from(level.saturating_sub(1).min(MAX_ATTRIBUTE_POINTS)),
                "level {level}: the spend a live character carries must equal min(level-1, 49)"
            );
        }
    }

    /// What report #109 actually costs its reporter.
    ///
    /// He is level 89 with 46 stamina and 3 magicka. The old `pool_for_level`
    /// handed him 640 of BOTH, ignoring the split; retail gives 660 and 230. His
    /// Ward costs 205 magicka, so retail lets him cast it once before he is dry
    /// and we let him cast it three times.
    #[test]
    fn the_reporters_pools() {
        assert_eq!(pool_for_points(46), 660, "his stamina");
        assert_eq!(pool_for_points(3), 230, "his magicka");
        // The control that makes those numbers mean something: the formula they
        // replace gave one value for both pools and it matched neither.
        assert_eq!(pool_for_level(89), 640);
        assert_ne!(pool_for_level(89), pool_for_points(46));
        assert_ne!(pool_for_level(89), pool_for_points(3));
    }

    /// A fighter with no character record — a bot, or the starter loadout — must
    /// NOT be read as "spent nothing" and handed a 200 pool. It keeps the level
    /// approximation until bots get real spends of their own.
    #[test]
    fn a_bot_keeps_the_level_approximation() {
        let lo = Loadout { level: 50, ..Default::default() };
        assert!(!lo.has_character, "the default loadout is not a character");
        assert_eq!(lo.stamina_points, 0);
        // If `has_character` were ignored, this bot would get 200 rather than 445.
        assert_ne!(pool_for_points(lo.stamina_points), pool_for_level(lo.level));
    }

    /// THE test that matters, and the one I nearly shipped without.
    ///
    /// The four above assert `pool_for_points` in isolation — and every one of
    /// them still passed with `Fighter::new` reverted to the old level formula.
    /// A helper that is correct while nothing calls it is exactly the state this
    /// module has been in before. This drives the real constructor.
    #[test]
    fn a_character_backed_fighter_gets_its_own_spend() {
        let now = std::time::Instant::now();
        // The report #109 reporter: level 89, 46 stamina, 3 magicka.
        let lo = Loadout {
            level: 89,
            stamina_points: 46,
            magicka_points: 3,
            has_character: true,
            ..Default::default()
        };
        let f = Fighter::new(0, 1, lo, now);
        assert_eq!(f.max_stamina, 660, "stamina from his own spend, not his level");
        assert_eq!(f.max_magicka, 230, "and magicka separately — the pools differ");
        assert_eq!(f.stamina, f.max_stamina, "a fighter starts full");
        assert_eq!(f.magicka, f.max_magicka);
        // The negative control: the formula this replaced gave one number for both.
        assert_ne!(f.max_stamina, f.max_magicka);
        assert_ne!(f.max_magicka, pool_for_level(89));
    }

    /// And the other half of the branch, through the constructor.
    #[test]
    fn a_bot_fighter_still_uses_the_level_approximation() {
        let now = std::time::Instant::now();
        let f = Fighter::new(0, 1, Loadout { level: 50, ..Default::default() }, now);
        assert_eq!(f.max_stamina, pool_for_level(50));
        assert_eq!(f.max_magicka, pool_for_level(50));
        assert_ne!(f.max_stamina, POOL_BASE, "not read as \"spent nothing\"");
    }
}

/// Approximate Stamina/Magicka pool for a level, for fighters with no character
/// record behind them — bots and the starter loadout. A real character uses
/// [`pool_for_points`] with its own spend.
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

/// `ActorAnimation` (`BGS.Game.Animation`, `dump.cs:12812`) — the animation a
/// maneuver plays, carried at propId 10 of op58 `PlayerManeuverStateChange`.
///
/// CAPTURE-PINNED. propId 10 is **constant per ability** across all 2,941 captured
/// op58 frames — 16 distinct maneuvers, not one of them varying — and every value
/// matches the same-named member of this enum: Power Attack 3, Quick Strikes 5,
/// Dodging Strike 6, Skullcrusher 12, Guardbreaker 13, Indomitable Smash 14,
/// Piercing Strikes 15, Venom Strikes 16, Recovery Strikes 17, Adrenaline Dodge 21,
/// Focusing Dodge 22, Renewing Dodge 23.
///
/// The one rule that is not "same name": all four shield bashes send
/// **`ShieldBashBegin` (26)**, not their own members (`ShieldBash` 4,
/// `HarryingBash` 18, `StaggeringBash` 19, `ReflectingBash` 20) — Shield Bash 61
/// frames, Staggering Bash 380, Harrying Bash 243, Reflecting Bash 7, all 26. That
/// matches both the class hierarchy (`AbilityDoShieldBash : AbilityDoManeuver`,
/// `dump.cs:604149`, with the other three deriving from it) and the shipped
/// description — *"The fighter first blocks with the shield"* — the begin
/// animation is the shared shield-raise every bash opens with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ActorAnimation {
    None = 0,
    Cast = 1,
    Channeling = 2,
    PowerAttack = 3,
    ShieldBash = 4,
    QuickStrikes = 5,
    DodgingStrike = 6,
    Notify = 7,
    Staggered = 8,
    Burning = 9,
    Paralyzed = 10,
    BreakObject = 11,
    Skullcrusher = 12,
    Guardbreaker = 13,
    IndomitableSmash = 14,
    PiercingStrikes = 15,
    VenomStrikes = 16,
    RecoveryStrikes = 17,
    HarryingBash = 18,
    StaggeringBash = 19,
    ReflectingBash = 20,
    AdrenalineDodge = 21,
    FocusingDodge = 22,
    RenewingDodge = 23,
    RecklessFury = 24,
    Breath = 25,
    ShieldBashBegin = 26,
    Concede = 27,
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
    /// A pointer-driven manual slash. Distinct from the tap/hold fallback
    /// `PlayerAutoAttack`; serialized in `PlayerAttackStateChange` (gmid 40).
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

/// The visual geometry for a pointer-driven slash.
///
/// Retail sends this only on `PlayerAttackStateChange` (gmid 40): a unit swipe
/// direction plus the screen-space point at which the slash was recognised. It is
/// presentation data only. Damage, side and charge remain server-authoritative.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ManualAttackGesture {
    pub direction: (f32, f32),
    pub execution_point: (f32, f32),
}

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
    /// Green-fog blindness on the VICTIM. **8** (`dump.cs:609805`). There is no
    /// blind ACTOR state — `ActorStateType.StateId` has none — so the fog is rendered
    /// entirely client-side off this status; the server's whole job is to send it.
    Blind = 8,
    Paralyzed = 9,
    StaggeredWeakness = 10,
    RecklessFury = 11,
    Dodging = 12,
    Firewall = 13,
    /// Ward negation buff (elemental-negation pool + armor). [§4.2]
    Ward = 15,
    /// The shield the three `*Armor` spells apply. **16** (`dump.cs:609812`) — ONE
    /// shared value: there is no per-element status, and `StormArmorAbility`
    /// (dump.cs:607176) is the single class backing Firestorm/Blizzard/Tempest. The
    /// element lives on the ability, not the status.
    ElementalStormArmor = 16,
    /// Absorb negation buff (damage→heal pool). [§4.1]
    Absorb = 17,
    Flying = 18,
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
    // The elemental WEAKNESS block. Wire-observed (58 op51 commands), but do NOT wire
    // effects off these yet: they arrive in a 13-prop extended op51 shape where
    // propId5 == propId8 and propId5 - propId12 == 96 in all 58 samples, so the
    // captures cannot decide whether 100-103 is a second enum block or whether the
    // extended shape moves the real type to propId 12. Both readings fit the data.
    FireWeakness = 100,
    FrostWeakness = 101,
    ShockWeakness = 102,
    PoisonWeakness = 103,
    HealthRegenReduction = 120,
    StaminaRegenReduction = 121,
    MagickaRegenReduction = 122,
    HealthRestoration = 170,
    StaminaRestoration = 171,
    MagickaRestoration = 172,
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
/// **PvP, not PvE.** This was `PlayerCombatParameters.postOptimalBlockResetTime`
/// (1.4 s), with a comment arguing that `PvpDefaultSettings`' 0.8 s "was a
/// server-cheat default". That was a considered choice, but it is inconsistent with
/// what this module already does: `ARENA_HEALTH_MULTIPLIER` IS
/// `PvpDefaultSettings.CHEAT_BASE_HEALTH_MULTIPLIER`, applied to every arena
/// fighter. Taking that class's health value while refusing its block value is the
/// position that needs defending. It is, by name and content, the PvP settings —
/// and the arena is PvP.
pub const OPTIMAL_BLOCK_RECOVERY_SECS: f32 = PVP_OPTIMAL_BLOCK_RECOVERY_TIME;

/// `PvpDefaultSettings.OPTIMAL_BLOCK_RECOVERY_TIME` (`dump.cs:427015`).
const PVP_OPTIMAL_BLOCK_RECOVERY_TIME: f32 = 0.8;

/// `PvpDefaultSettings.BASE_STAGGER_DURATION` (`dump.cs:427016`). The PvE value in
/// `CombatParameters` is 1.5 s; arena is PvP and staggers for longer.
const PVP_BASE_STAGGER_DURATION: f32 = 2.5;

/// The block phase for a defending fighter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockPhase {
    /// Guard within `BLOCK_OPTIMAL_TIME_SECS` of going up, raised outside the
    /// post-release cooldown: the block uses `R × (1 + OptimalBlockBoost)`.
    Optimal,
    /// A LOW block: held past the optimal window, or raised inside the cooldown. An
    /// ordinary block at `×1 R` that carries NO wire flag. (The name predates the
    /// client read; the client's "late block" is a different thing, a hit that lands
    /// before the guard's startup finishes and is not mitigated at all — 03 §2.7.)
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

/// The packed pools + sequence id carried at propIds 4/5 of `ReceiveDamage` (50),
/// `PlayerStatsUpdate` (65) and every member of the `PlayerStateChange` family.
///
/// ```text
///   bits 63..32  sequenceId   (a per-round monotonic counter, shared by p4 and p5)
///   bits 29..20  HEALTH       10-bit fraction of max, STAT_MAX = full
///   bits 19..10  STAMINA
///   bits  9..0   MAGICKA
/// ```
///
/// ## Health and Magicka were the wrong way round until 2026-08-02
///
/// This used to write `Health | Stamina<<10 | Magicka<<20`, i.e. health in the LOW
/// ten bits. The half-split (stats high, seq low) was capture-verified; the order of
/// the fields *within* the stat word never was, and it was backwards. Every
/// `ReceiveDamage` we have ever sent has been showing the client our health value as
/// its magicka bar, and vice versa.
///
/// Two independent capture signals fix it, both from prod session 503, decoded by
/// walking every ENet command in the packet (labelling by the packet's first command
/// hides ~40 % of these frames):
///
/// **1. Health is bits 20-29 — it is the field that empties to exactly 0 at death.**
/// Avatar 91's op50 track, one round:
///
/// ```text
///   990 → 972 → 835 → 643 → 555 → 488 → 396 → 305 → 120 → 84 → 26 → 0
/// ```
///
/// then it resets to 814 for the next round and walks down again. Bits 0-9 and 10-19
/// oscillate over the same frames (517 → 350 → 207 → 233 → 403 …), because those
/// pools regenerate at ~5 %/s.
///
/// Note the careful wording. Health never rises *in this trace*, but that is a
/// property of this fighter, **not of the game**: a regen perk with the right
/// rings/armour does recover health mid-round. It is rare and usually too slow to
/// notice, which is why the trace looks monotone. The load-bearing evidence is the
/// terminal zero — on the killing blow health reads 0 while stamina reads 22 and
/// magicka 240, and only one pool is the one that empties at death.
///
/// **2. Magicka is bits 0-9 — it is what a cast spends.** Same avatar: frame 3503950
/// is a `PlayerChannelingStateChange` (53), the start of a cast, and the very next
/// stat word has bits 0-9 dropping 1023 → 531 while bits 20-29 barely move
/// (992 → 972). Stamina is then the only slot left, at bits 10-19 — consistent with
/// it reading 0 through the heavy-melee stretches and refilling between them.
///
/// Soft third check, from the s506 byte-differential below: the damaged fighter in
/// `receive_damage_matches_capture` took 85.17 health damage and a 24.44 magicka
/// drain. Read this way it is health 925 / magicka 914 — the big pool losing the
/// smaller *fraction*, which is what an arena ×3 health pool should do. The old
/// reading had it backwards.
pub struct PackedStats;

impl PackedStats {
    pub fn pack(health: u16, stamina: u16, magicka: u16, seq: u32) -> u64 {
        let h = (health.min(STAT_MAX) as u64) & 0x3ff;
        let s = (stamina.min(STAT_MAX) as u64) & 0x3ff;
        let m = (magicka.min(STAT_MAX) as u64) & 0x3ff;
        let stats = m | (s << 10) | (h << 20);
        (stats << 32) | (seq as u64) // stats in the HIGH 32, sequence id in the LOW 32
    }

    /// Returns `(health, stamina, magicka, seq)`.
    pub fn unpack(v: u64) -> (u16, u16, u16, u32) {
        let stats = (v >> 32) as u32;
        let magicka = (stats & 0x3ff) as u16;
        let stamina = ((stats >> 10) & 0x3ff) as u16;
        let health = ((stats >> 20) & 0x3ff) as u16;
        let seq = (v & 0xffff_ffff) as u32;
        (health, stamina, magicka, seq)
    }

    /// Bit offset of the HEALTH field inside the full 64-bit word — for the places
    /// that read health straight out of a wire value instead of going through
    /// [`Self::unpack`].
    pub const HEALTH_SHIFT: u32 = 52;
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
    /// Weapon weight class — picks the standard timings/factors when no template
    /// resolved (`Loadout::charge_params`).
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
    /// Character level — drives max-Health (`health_for_level`).
    pub level: u16,
    /// Flat maximum-Health granted by equipped `FortifyHealthPropertyLogic`
    /// enchantments. Arena triples the character's resulting Health pool, including
    /// this primary enchantment; omitting it left a level-86 fighter with two tier-10
    /// Health enchants at 2,070 instead of about 2,734.
    pub max_health_bonus: f32,
    /// Attribute points this character spent on Stamina. Max Stamina is
    /// [`pool_for_points`] of this, NOT a function of level — see that function
    /// for the extraction and the identity check. 0 for a bot or the starter
    /// loadout, which fall back to [`pool_for_level`].
    pub stamina_points: u16,
    /// Attribute points this character spent on Magicka.
    pub magicka_points: u16,
    /// True when this loadout came from a real character record, so the spends
    /// above are its own. A bot has no record and must not be handed a 200-point
    /// pool just because it spent nothing.
    pub has_character: bool,
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
    /// Every equipped item PROPERTY id, one entry per occurrence, across ALL equipped
    /// items — weapon, armour, jewellery alike.
    ///
    /// This is what **Enchantment Synergy** actually keys on. Disassembled:
    /// `ItemPropertyBonusInstance::GetXValueMultiplier` calls
    /// `ActorBonusHandler.HasMatchingItemBonus(propertyId)`, which walks
    /// `_bonusesByItem` across every equipped item and returns `matchCount > 1`. So
    /// "stacked" means **two or more copies of the SAME specific property**, counted
    /// actor-wide — not two enchants of the same element, and not per item.
    ///
    /// We keyed it on `DamageType` and looked only at the weapon's own enchant list,
    /// which was wrong in both directions at once: "Weapon Fire Damage" plus "Fortify
    /// Fire Damage" counted as stacked when they are different properties, while two
    /// genuinely identical properties on a helmet and a cuirass counted as nothing.
    pub property_ids: Vec<uuid::Uuid>,
    /// The property id behind each entry of [`Loadout::enchants`], positionally
    /// aligned. Kept beside `enchants` rather than folded into it so the many test
    /// fixtures that build `enchants` directly keep working; an empty vector simply
    /// means "no synergy information", never "not stacked".
    pub enchant_property_ids: Vec<uuid::Uuid>,
    /// Attacker-side **Armor Piercing Rating**
    /// (`ArmorPiercingPhysicalPropertyLogic`) — subtracted from the defender's
    /// Armor Rating before the physical reduction. [Phase 3.3]
    pub armor_piercing_rating: f32,

    /// Flat bonus damage contributed by the MANEUVER being resolved, already scaled
    /// by the grip multiplier. Zero for an ordinary swing.
    ///
    /// Every maneuver rank ships `parameters.bonusDamage` plus
    /// `oneHandedMultiplier` / `twoHandedMultiplier`, and NOTHING read them — the
    /// generated tables carried the numbers and the resolver never looked. That is
    /// why every maneuver resolved to a plain Middle weapon hit: Power Attack's
    /// 75.33, Skullcrusher's 100.09, Guardbreaker's 131.15 were all silently 0.
    ///
    /// Set on a CLONE of the attacker's loadout for the duration of one cast (the
    /// same trick the piercing ratings already use), so a maneuver cannot leak its
    /// bonus into the next auto-attack.
    pub maneuver_bonus_damage: f32,

    /// **Powerful Block** — `PowerfulBlockPropertyLogic`, enchantment
    /// `f8e9dec5-c6e7-4976-b24b-2155f1921692`.
    ///
    /// "Target stunned by a blocked attack takes {0} extra damage while stunned."
    /// This is the WEAKNESS RATING the wearer inflicts on an attacker whose swing
    /// their block stunned — it is NOT block rating, which is what it was being
    /// parsed as. Tier magnitudes 18.0 .. 50.4.
    pub powerful_block: f32,

    /// Reserved for weapon-alchemy poison status effectiveness. Venom Strikes ships
    /// `_poisonEffectIncrease`, but retail applies that to poison effects/duration,
    /// not to direct Poison damage components.
    pub poison_effect_multiplier: f32,

    /// Resolved perk bonuses, computed once at parse time. `Default` (every
    /// field zero) for a fighter with no perks, which every application site
    /// treats as a no-op.
    pub perks: super::perks::PerkBonuses,
    /// Attacker-side `Fortify <Element> Damage` — a 0..1 fraction per element that
    /// raises that element track's amplification ceiling. [Phase 3.6]
    pub element_fortify: Vec<(DamageType, f32)>,
    /// Elemental RETALIATION from gear: when this fighter is hit, each entry deals
    /// that much of that damage type back at the attacker (`DamageSource::Revenge`).
    ///
    /// Capture-measured, not inferred: across 203 Revenge frames in s615/s616 the
    /// damage type varies per wearer (Frost / Fire / Poison), the magnitudes repeat
    /// from a small fixed set, and they do NOT scale with the incoming hit — 105.0
    /// followed a blocked 54.3 and again a blocked 23.8. It is the wearer's gear
    /// hitting back, not a block-punish.
    pub revenge: Vec<(DamageType, f32)>,
    /// Gear that damages the wearer's OPPONENT continuously, whether or not anyone
    /// swings: `ContinuousPoisonDamagePropertyLogic` (Ebony Mail) and
    /// `ContinuousFrostDamagePropertyLogic` (Rimelink). Each entry is a damage type
    /// and its shipped rate in damage PER SECOND (`_xValueByTier`); the tick delivers
    /// `rate x CONTINUOUS_AREA_TICK_SECS`. See `resolve::apply_continuous_area_damage`.
    pub continuous_damage: Vec<(DamageType, f32)>,
    /// `Weapon`/`Shield` `Ravage{Stamina,Magicka,Health}` — the flat amount this hit
    /// takes off the victim's **maximum** pool ("Reduces target's maximum Stamina by
    /// {0}"), per landed swing. Distinct from the drain families, which take the
    /// CURRENT pool: a drain regenerates back, a ravage does not.
    ///
    /// Weight-scaled: at tier 10 a light weapon ravages 31.66, a versatile 42.0 and a
    /// heavy 52.66 — the three curves the shipped family carries.
    pub ravage: Vec<(DamageType, f32)>,
    /// `Shield Ravage {Stamina,Magicka,Health}` — the SHIELD families, which fire on a
    /// different trigger: *"on a blocked attack or Shield Bash"*. They ravage whoever
    /// attacked into the guard, not the target of a swing, so they are held apart from
    /// [`Loadout::ravage`] and applied from the defender's side.
    ///
    /// A shield has no weapon weight, so these ship a single curve — 52.66 at tier 10.
    pub shield_ravage: Vec<(DamageType, f32)>,
    /// Display name + character UUID for the round-start op50 spawn. Empty for the
    /// starter loadout (no character row); set by `loadout::from_character` + the
    /// matchmaker's character load.
    pub display_name: String,
    pub character_uuid: String,
    /// The two op54 round-start PROFILE JSON blobs (gear + full character), serialized
    /// from the stored character by the matchmaker; empty for the starter loadout.
    pub profile_equipped_json: String,
    pub profile_character_json: String,
    /// Pre-match economy snapshot used to build the same inventory/wallet diff the
    /// asynchronous PostgreSQL writer will persist after op49 is sent.
    pub wallet_gold: u64,
    pub character_experience: u64,
    pub backpack_version: u64,
    pub treasury_version: u64,
    pub next_treasury_chest_id: u64,
    pub stackable_counts: HashMap<uuid::Uuid, u64>,
    /// Hidden, season-independent position in the shipped 100-chest PvP cycle.
    pub arena_chests_earned: u64,
    pub arena_last_elder_one_chest_at_secs: i64,
    pub arena_last_elder_two_chest_at_secs: i64,
    pub arena_last_legendary_chest_at_secs: i64,
    /// The REST sequence echoed by retail's match-end card.
    pub current_request_index: i64,
    /// The client's own `HideHelmet` avatar toggle, as reported by its c2s op61
    /// `LoadoutClientBackendSynchronized`. Relayed to the OPPONENT in the op54
    /// profile (propId 6) so both players see the same avatar.
    pub hide_helmet: bool,

    // --- Rating-derived defence (Phase 3.3/3.4/3.5) ---
    /// Summed **Armor Rating** of every equipped armor piece
    /// (`gamedata::armor_rating`). Reduces PHYSICAL damage only, via
    /// `tables::armor_reduction` (`rating × reductionPerArmorRating`, capped at
    /// `maximumArmorReduction`). 0.0 for a fighter with no resolvable armor.
    pub armor_rating: f32,
    /// The **blocking item's** rating `R0`: the shield if one is equipped, otherwise
    /// the weapon — never the two summed (`InventoryUtility$$GetEquippedBlockingItem
    /// @0x1e61f50`). It is the item's temper-level block value when tempered, else its
    /// `blockBase`, ceiled (`get_EquippedBlockRating@0x1c54420`). See
    /// [`crate::arena::combat::loadout::blocking_item_rating`].
    pub block_rating: f32,
    /// Flat Block Reduction enchant rating. It adds to `R` AFTER the optimal boost
    /// (`Actor$$ResolveBlocking@0x1c54284` boosts the equipped rating only, then the
    /// `BlockRatingMethod` sources add). Type-agnostic for now; the per-type split is
    /// a separate gap (03-D11).
    pub block_rating_bonus: f32,
    /// The shield's `optimalBlockBoost` as shipped (1.0 on all 51 shields).
    pub shield_optimal_block_boost: f32,
    /// The equipped shield's `_damageBase`: a shield bash's base damage, all of it
    /// Bashing (`Actor$$get_ShieldDamage@0x1c5b250`, `ShieldTemplate.DamageType` = 3).
    /// 0 when no shield resolved.
    pub shield_damage: f32,
    /// **Elemental Resistance Piercing RATING** (not a fraction) contributed by the
    /// attacker's enchants — subtracted from the defender's resistance rating before
    /// the reduction is computed. [Phase 3.4]
    pub elem_resist_piercing_rating: f32,
    /// **PDOC / EDOC** — `Opportunist{Physical,Elemental}PropertyLogic`, flat damage
    /// "against targets suffering a condition".
    ///
    /// The gate is not a guess: the shipped asset carries
    /// `_triggerStatusEffects = [4, 5, 6, 7]` — Burning, Frozen, Enervated, Poisoned,
    /// the elemental four. Staggered, Blind and Paralyzed do NOT count.
    pub opportunist_physical: f32,
    pub opportunist_elemental: f32,
    /// Flat BLOCK-piercing ratings, subtracted from the defender's Block Rating before
    /// the block reduction is computed — the block-stage mirror of
    /// `armor_piercing_rating`. `block_piercing_rating` applies to physical,
    /// `elem_block_piercing_rating` to elemental. Skullcrusher ships 60.00 for the
    /// former, PiercingStrikes 122.40 for the latter. Zero for every other attack, so
    /// the block stage is unchanged unless an ability sets them.
    pub block_piercing_rating: f32,
    pub elem_block_piercing_rating: f32,
    /// The caster's Paralyze ability rank (0 = not equipped) — selects the shipped
    /// `_damageToCauseParalyze` / `_duration` row. [Phase 3.9]
    pub paralyze_rank: u8,
    /// The equipped WEAPON's `optimalBlockBoost` as shipped (1.0 on 363 of 370
    /// weapons; 0.5, 0.0 and 2.0 on the rest).
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
    /// The blocking item's `OptimalBlockBoost`: the shield's when a shield is
    /// equipped, otherwise the weapon's (`InventoryUtility$$GetEquippedBlockingItem
    /// @0x1e61f50`; `get_OptimalBlockBoost@0x1c5b8d8`).
    pub fn blocking_item_boost(&self) -> f32 {
        if self.has_shield {
            self.shield_optimal_block_boost
        } else {
            self.weapon_optimal_block_boost
        }
    }

    /// Commit-to-commit swing cadence (Phase 3.12): the equipped template's own
    /// `attackDelay + recoveryToComboTime`, floored at `globalMinimumAttackDelay`; the
    /// weight-class fallback when no template resolved.
    pub fn swing_interval(&self) -> std::time::Duration {
        use crate::arena::combat::tables;
        match self.weapon_template {
            Some(w) => tables::swing_interval_for_weapon(w),
            None => tables::fallback_swing_interval(self.weapon.weight.unwrap_or(tables::Weight::Light)),
        }
    }

    /// Commit-to-neutral actor-state timing. A new combo may legally begin before
    /// this expires; it is therefore not interchangeable with [`Self::swing_interval`].
    pub fn neutral_interval(&self) -> std::time::Duration {
        use crate::arena::combat::tables;
        match self.weapon_template {
            Some(w) => tables::neutral_interval_for_weapon(w),
            None => self.swing_interval(),
        }
    }

    /// The swing timings and factors of the equipped weapon (combat-spec 02 §2.2-§2.3,
    /// §4.2, §5.1). Read from the shipped template; a fighter whose weapon did not
    /// resolve (bot / starter fallback) gets its weight class's standard template
    /// values, which 363 of the 370 shipped templates share exactly (02 §2.3).
    pub fn charge_params(&self) -> ChargeParams {
        use crate::arena::combat::tables::Weight;
        if let Some(w) = self.weapon_template {
            return ChargeParams {
                attack_delay: w.attack_delay,
                backswing_time: w.backswing_time,
                max_damage_time: w.max_damage_time,
                min_damage_factor: w.min_damage_factor,
                max_damage_factor: w.max_damage_factor,
                combo_damage_factor: w.combo_damage_factor,
                recovery_time: w.recovery_time,
            };
        }
        // attackDelay / backswing / maxDamageTime / maxF / comboDF / recoveryTime of
        // the standard Light, Balanced and Heavy templates (02 §2.3, §4.2, §5.1).
        let (ad, bs, mdt, maxf, combo, rt) = match self.weapon.weight {
            Some(Weight::Heavy) => (0.40, 0.25, 0.035, 0.987, 0.186, 1.00),
            Some(Weight::Versatile) => (0.30, 0.20, 0.035, 0.625, 0.25, 0.80),
            _ => (0.233_333, 0.116_667, 0.035, 0.325, 0.54, 0.55),
        };
        ChargeParams {
            attack_delay: ad,
            backswing_time: bs,
            max_damage_time: mdt,
            min_damage_factor: 0.0,
            max_damage_factor: maxf,
            combo_damage_factor: combo,
            recovery_time: rt,
        }
    }
}

/// The per-weapon inputs of the client's charge clock, combo factor and recovery
/// gates (`WeaponTemplate +0xe4..+0xfc`, combat-spec 02 §2.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChargeParams {
    pub attack_delay: f32,
    pub backswing_time: f32,
    pub max_damage_time: f32,
    pub min_damage_factor: f32,
    pub max_damage_factor: f32,
    /// `_comboDamageFactor`: added once the chain is >= 1 (0.54 / 0.25 / 0.186).
    pub combo_damage_factor: f32,
    /// Recovery -> Idle, which ends the chain (02 §4.4, R1).
    pub recovery_time: f32,
}

impl ChargeParams {
    /// `PlayerActor$$get_MinDamageTime@0x18693b8`: `max(attackDelay*s, 0.1)`. A release
    /// before it is not a swing (`AttackChargeState$$IsAttackPossible@0x1a4eecc`).
    pub fn min_damage_time(&self, s: f32) -> f32 {
        (self.attack_delay * s)
            .max(crate::arena::combat::gamedata::combat_params::GLOBAL_MINIMUM_ATTACK_DELAY)
    }

    /// `get_DecayStartTime@0x186958c`: `MinDamageTime + backswing*s`, the plateau's end.
    pub fn decay_start_time(&self, s: f32) -> f32 {
        self.min_damage_time(s) + self.backswing_time * s
    }

    /// `get_MaxDamageTime@0x18694b0`: `DecayStartTime - maxDamageTime*s`, where the
    /// plateau starts. (The getter and the field of the same name mean different
    /// things: the field is the plateau's length.)
    pub fn plateau_start_time(&self, s: f32) -> f32 {
        self.decay_start_time(s) - self.max_damage_time * s
    }

    /// The swing factor a release after `t` seconds of charge is worth, or `None` when
    /// the release is too early to be a swing at all (`AttackFailed`).
    ///
    /// `AttackChargeState$$GetDamageFactor@0x1a4ecb8` with
    /// `PlayerActor$$get_ShouldLerpDamageFactor@0x1862e18` = false: a step, not a ramp.
    /// `maxDamageFactor` on the plateau `(MaxDamageTime, DecayStartTime]` only, and
    /// `minDamageFactor` before and after it. `s` is the weapon-speed multiplier.
    pub fn swing_factor(&self, t: f32, s: f32) -> Option<f32> {
        let min = self.min_damage_time(s);
        if t < min {
            return None;
        }
        let f = if t <= min {
            0.0
        } else if t > self.plateau_start_time(s) && t <= self.decay_start_time(s) {
            self.max_damage_factor
        } else {
            self.min_damage_factor
        };
        Some(f)
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
    /// One tick's share of `_percentHealthDamage × baseMaxHP` over the effect's
    /// authored duration; 0.0 for non-DoT effects. Game-data-driven — the observed
    /// s506 range is 1.25–7.73 damage/tick.
    pub per_tick_damage: f32,
    pub expires_at: Instant,
    pub last_tick: Instant,
    /// True for a Resist-Elements transient resistance — these are carried in
    /// `ActiveEffect` rather than the permanent `Loadout.resistances` so they
    /// auto-expire and are cleaned up without touching the loadout.
    pub is_transient_resist: bool,
}

/// A channelled (`_damagePerSecond`) spell still delivering ticks.
///
/// Retail streams a channel as a run of `ReceiveDamage` frames carrying
/// `DamageSource::ContinuousSpell (8)`, one every
/// [`damage::CHANNEL_TICK_INTERVAL_SECS`], for `channelMaxLength` seconds. This is the
/// server-side schedule for the ticks after the first — the cast itself emits tick 1
/// inline, exactly as a swing emits its own hit.
///
/// Only the identity of the cast is stored, never a precomputed damage number: each
/// tick re-enters `resolve_ability`, so block, resistance and the mirrored stamina
/// drain are all re-read against the target's state AT THAT TICK. A channel that
/// starts against an unblocked target and ends against a raised guard is reduced for
/// the part that lands late, which is what a streamed channel means.
#[derive(Debug, Clone)]
pub struct ActiveChannel {
    pub caster_slot: usize,
    pub target_slot: usize,
    pub ability_uuid: String,
    pub ability_level: u8,
    /// Ticks still owed, EXCLUDING the one the cast already emitted.
    pub remaining_ticks: u32,
    pub next_tick_at: Instant,
    /// Was the caster's magicka full at CAST time? Maximum Power's condition is
    /// evaluated once, when the spell goes off — the cast itself spends magicka, so
    /// re-reading it per tick would turn the perk off for every tick but the first.
    pub magicka_full_at_cast: bool,
    /// Seconds between this channel's ticks.
    ///
    /// Almost always the 0.2 s global PvP tick, but THUNDERSTORM is not a channel in
    /// the DoT sense: it ships `_numberOfBolts` 3 over a `_duration` of 9 s, i.e. one
    /// bolt every 3 s. The interval is DERIVED (duration / bolts) — no interval field
    /// is authored — and it is per-channel because a hardcoded global tick turned
    /// Thunderstorm into a single immediate hit.
    pub interval_secs: f32,
    /// When the cast that started this channel was accepted; matches
    /// [`Execution::started_at`] for the two interruptible channels.
    pub cast_at: Instant,
}

// ---------------------------------------------------------------------------
// Per-fighter authoritative state
// ---------------------------------------------------------------------------

/// A potion that is still taking effect.
///
/// `remaining` is what is left to give, `per_tick` what each regen tick hands
/// over. Both are floats because a 225-point restoration over 2.5 s does not
/// divide evenly into whole points per second, and rounding each tick
/// independently would quietly lose several points of the potion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingRestore {
    /// `ActorStats.CoreStats`: 0 Health, 1 Stamina, 2 Magicka.
    pub affected_stat: u8,
    pub remaining: f32,
    pub per_tick: f32,
}

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
    /// How much maximum Stamina / Magicka ravage has removed **this round**.
    ///
    /// Held separately from `max_stamina`/`max_magicka` so the round reset can hand
    /// the ceiling straight back: ravage does not cross a round boundary. Nothing is
    /// sent for it — the wire carries pools as fractions of max, so a reduced ceiling
    /// is invisible to the bar by construction. See `charge_initial_cooldowns` for
    /// the other per-round reset.
    pub ravaged_stamina: u32,
    pub ravaged_magicka: u32,
    pub ravaged_health: u32,
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
    /// weapon's own `attackDelay + recoveryToNeutralTime`. Firing all four the instant the
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
    /// A cast pose announced by op53 that the client will hold until a state message
    /// ends it: `(when the channel ends, transitions_total right after op53 recorded
    /// Channeling)`. See [`Self::begin_channel_pose`].
    channel_pose: Option<(Instant, u32)>,
    pub state_entered: Instant,
    /// Slot of the implicit arena target (the opponent) for `RequestExecuteAbility`.
    pub arena_target: usize,
    /// The facing the guard animation was raised on — retail's
    /// `PlayerBlockingState.Parameters.ActiveSide` (`dump.cs:597100-597103`), which is
    /// `Middle` in 578 of 578 recorded blocking-state frames.
    ///
    /// **Presentational only. It is NOT a hit-test** (tracker #31): high vs low
    /// blocking is a timing phase ([`Fighter::block_phase`]), never a direction, so
    /// `damage::block_outcome` does not compare it against the attacker's side. The
    /// field is kept because it mirrors a real wire property and is still set on both
    /// block-raise paths; nothing in the damage pipeline reads it.
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
    /// For a BOT: how many times it has cast each ability this match, keyed by
    /// instance UUID. Drives least-cast-first selection so a bot match exercises the
    /// whole loadout instead of hammering whichever ability happens to sort first.
    pub bot_cast_counts: std::collections::HashMap<String, u32>,
    /// For a BOT: when it last cast an ability, throttling casts independently of the
    /// swing cadence so it does both rather than one or the other.
    pub bot_last_cast: Option<Instant>,
    /// Server-side timestamp when this fighter last pressed the attack button (op46
    /// `_held=1`). Used with the release timestamp (op46 `_held=0`) to compute the
    /// server-measured hold duration for the held-charge crit gate (bug 4).
    /// `None` ⇒ no press in progress (e.g. bot swings, or between attacks).
    pub charge_press_at: Option<Instant>,
    /// **Combo state** (combat-spec 02 §4). `Actor._comboCount`: the number of
    /// connected weapon hits in the current chain. A hit reads it before its own
    /// increment, and `>= 1` adds `_comboDamageFactor` once
    /// (`damage::attack_type_multiplier`). Advanced when a hit LANDS
    /// ([`Self::increment_combo`]); a swing that repeats a side starts a fresh chain
    /// ([`Self::begin_combo_swing`]); reset by the §4.3 events (going Idle after
    /// `recoveryTime`, a guard, a spell, a stagger, a paralysis, a midline crossing
    /// during the hold, a maneuver's end, a round start).
    pub combo_count: u32,
    /// The `ActiveSide` of this fighter's last combo-counting swing (Left/Right), so
    /// the next swing can tell an *alternating* chain (combo++) from a repeat (reset).
    /// `None` at round start / after a reset.
    pub last_combo_side: ActiveSide,
    /// The side of the last committed swing, kept through a combo reset. The
    /// Recovery gates compare the next charge's side with THIS swing's side
    /// (`PlayerRecoveryState$$CanBeginCombo@0x1d75980`), whatever the chain did.
    pub last_swing_side: ActiveSide,
    /// When the current charge began (op46 DOWN, or a bot's wind-up). The chain
    /// survives only if the next charge BEGINS inside Recovery (combat-spec 02 §4.4).
    pub charge_began_at: Option<Instant>,
    /// When the last swing's Recovery began: the FollowThrough beat plus one frame.
    /// `recoveryTime` after it the actor goes Idle and the chain ends (02 R1, X5).
    pub recovery_began_at: Option<Instant>,

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
    /// Manual-slash geometry recognised from the current input path. The client
    /// explicitly marks a gmid-47 sample `IsStartAttackTriggerReady`; we retain the
    /// matching segment until the release chooses gmid 40 instead of gmid 52.
    pub pending_manual_attack: Option<ManualAttackGesture>,
    /// Geometry belonging to an already committed `PlayerAttack` transition. Kept
    /// through the actor-state outbox drain, then cleared after gmid 40 is built.
    pub active_manual_attack: Option<ManualAttackGesture>,
    /// The **client-reported** charge/hold duration in seconds (gmid 46 propId 5, and
    /// the same value latched into gmid 47 propId 7).
    ///
    /// **TELEMETRY ONLY — never authoritative.** This is client-authored and therefore
    /// trivially spoofable (a modified client could report a full charge on every tap
    /// and crit every swing). The crit gate uses the *server*-measured hold
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
    /// loadout. Read by `transient_resistance_against`, which the damage pipeline calls
    /// AFTER block (the same insertion point as loadout resistances). Duration is read
    /// per rank from `ResistElementsAbility._resistanceDuration` (10 s at rank 1).
    pub transient_resistances: Vec<(DamageType, f32, Instant)>, // (type, flat_amount, expires_at)
    /// Resistance against EVERY damage type, as (flat_amount, expires_at).
    /// Willpower grants this for the duration of a spell cast (and Indomitable
    /// Smash's `_bonusResistance` rides it); unlike `transient_resistances` it is
    /// not keyed by type, because the shipped text is "Resistance to all damage".
    /// Combat Focus is NOT pushed here — see [`Fighter::combat_focus_active`].
    pub transient_all_resistance: Vec<(f32, Instant)>,
    /// The fighter is in the client's `Maneuver` actor state until this instant.
    ///
    /// The resolver does not move its logical `actor_state` into `Maneuver` (op58
    /// only records it presentationally), so this carries the one fact a perk
    /// needs: Combat Focus pays out while the actor is in the Maneuver state
    /// (`CombatFocusPerk$$GetResistanceBonus@0x1A5AA60`). Set when a maneuver cast
    /// is accepted, for the same castingDelay + channel (>= 0.5 s) window the fork
    /// has always used to approximate the maneuver's animation.
    pub maneuver_state_until: Option<Instant>,
    /// While set and in the future this fighter is STAGGERED
    /// (`CombatParameters.baseStaggerDuration` 1.5 s): inputs are dropped, exactly
    /// like `Paralyzed`, and the actor-state is `Staggered`. [Phase 3.13]
    pub staggered_until: Option<Instant>,

    /// **Reckless Fury** is active until this instant. `RecklessFuryAbility`
    /// serializes only `_bonusDamages` and `_duration` (5.0 s at every rank), so the
    /// four headline behaviours its description promises — cannot be stunned, cannot
    /// die, cannot block, cannot use skills — are boolean state tied to
    /// `StatusEffectType::RecklessFury (11)` for that window, not numbers to read.
    ///
    /// Before this existed the ability had NO persistent implementation at all: it
    /// was resolved as one ordinary Middle weapon hit. That is the whole of the
    /// production report in match fffe01ca — the AI cast Fury and was stunned 2 s
    /// later, inside the window that is supposed to make it un-stunnable.
    /// **Magicka Surge**: a flat magicka-per-second bonus until this instant, then a
    /// blackout window in which magicka does not regenerate at all.
    ///
    /// `MagickaSurgeAbility` ships `_magickaRegenerationBonus` (81.84/s at rank 1),
    /// `_duration` (10 s) and `_noMagickaRegenDuration` (10 s) — the drawback that
    /// pays for the surge. All three were unread, so the spell cost magicka and did
    /// nothing whatsoever.
    /// **Reflecting Bash**: while this instant is in the future, damage the defender
    /// takes is redirected back at its attacker, up to `reflect_remaining`.
    ///
    /// `_damageReduction` (110.67 at rank 1) is the cap on damage "absorbed AND
    /// redirected". The reduction half was already applied as a transient resistance;
    /// the REDIRECT half was never implemented, so Reflecting Bash was strictly worse
    /// than Shield Bash — same guard, less damage, no reflection.
    /// **Wall of Fire** is up until this instant. While it is, an attacker who lands
    /// a hit on this fighter "passes through" the wall and is burned for
    /// `firewall_damage`; the caster pays `firewall_self_pct` of that in fire itself.
    ///
    /// `FirewallAbility` ships `_damage` 93.33 **per attack passing through** (not per
    /// second), `_duration` 5 s and `_selfDamagePercent` 0.2. The server resolved it
    /// as one immediate hit and there was no wall at all.
    pub firewall_until: Option<Instant>,
    pub firewall_damage: f32,
    pub firewall_self_pct: f32,

    /// **Echo Weapon** is up until this instant; each landed weapon hit is echoed
    /// `echo_delay` later for `echo_bonus` flat damage.
    ///
    /// `_duration` 8 s, `_weaponDelay` 0.5 s, and a per-weapon-class `_bonusDamages`
    /// (Light 95.84 / Versatile 108.03 / Heavy 123.70). None of it was implemented —
    /// the spell produced no echoes whatsoever.
    pub echo_until: Option<Instant>,
    pub echo_bonus: f32,
    pub echo_delay: f32,

    /// Weakness rating this fighter is currently suffering from
    /// `StatusEffectType::StaggeredWeakness` — the attacker holds it, and it
    /// amplifies damage it TAKES. Zero when the status is not active.
    ///
    /// The status has no duration of its own (every captured op51 carries 0.0) and is
    /// removed with the Staggered that produced it, so this is cleared there.
    pub weakness_rating: f32,

    pub reflect_until: Option<Instant>,
    /// Remaining redirect budget for the current Reflecting Bash window.
    pub reflect_remaining: f32,

    pub magicka_surge_until: Option<Instant>,
    /// Flat magicka/second granted while `magicka_surge_until` is in the future.
    pub magicka_surge_bonus: f32,
    /// No magicka regenerates at all until this instant — Magicka Surge's drawback,
    /// which begins when the surge itself ends.
    pub no_magicka_regen_until: Option<Instant>,

    pub reckless_fury_until: Option<Instant>,
    /// Flat bonus damage Fury adds to each swing while active, chosen by the
    /// wielder's WEAPON CLASS from the rank's `bonusDamages` table
    /// (Light 11.24 / Balanced 14.17 / Heavy 17.86 at rank 1).
    pub reckless_fury_bonus: f32,

    /// Statuses we have told the clients are ACTIVE on this fighter, as of the
    /// last tick.
    ///
    /// The engine emitted op51 applies and never a single remove — 15 call
    /// sites, all `apply = true`. Retail sends a remove for every status: across
    /// 2,889 op51 messages in s615+s616 the apply/remove counts run ~1:1 for all
    /// nineteen effect types (Staggered 132/140, Paralyzed 16/17, Blocking
    /// 736/756). The client does NOT time an effect out from the duration it was
    /// given, so without the remove the visual sticks forever — a player watched
    /// a bot swing at him while still rendered mid-stun.
    ///
    /// Diffing this against the live state each tick is what generates the
    /// removes, rather than emitting one at each expiry site: expiry happens in
    /// three different places (`effects` pruning, `reconcile_stagger`,
    /// `reconcile_paralysis`), two of which run from input handlers that cannot
    /// reach the wire.
    ///
    /// Deliberately holds ONLY the statuses [`Self::tracked_statuses`] can see: a
    /// status the scan cannot see would make the very first diff emit a bogus
    /// remove for a status that had just been applied. Every status we announce
    /// is now visible to it (Ward / Absorb / storm armor through their pools,
    /// 60-63 and FlashFreeze's Frozen through `status_timers`); Blocking is synced
    /// separately, see `announced_blocking`.
    announced_statuses: Vec<StatusEffectType>,
    /// Statuses whose lifetime no other field models, as `(status, expires_at)`:
    /// Resist Elements' 60-63 and FlashFreeze's Frozen (5). They exist only so
    /// [`Self::tracked_statuses`] can see them lapse and the diff sends their op51
    /// remove. A PvP client never times a status out itself
    /// (`PvpPlayerActor$$RemoveStatusEffect@0x1a32aa8` removes only on `force`), so
    /// without the remove the resistance icon and FlashFreeze's local Slow stay up
    /// until the round clears them.
    pub status_timers: Vec<(StatusEffectType, Instant)>,
    /// Whether the clients were last told this fighter holds `Blocking` (1).
    ///
    /// Blocking is announced both ways from the guard itself, not from an apply
    /// site, because a guard is raised and dropped on a dozen paths (op46 press and
    /// release, bot guard, bash window, stagger, paralysis, Fury, window expiry).
    /// Kept out of `announced_statuses` so the removal diff can never absorb a raise
    /// silently. See [`Self::sync_blocking_status`].
    announced_blocking: bool,
    /// Consumables used in the CURRENT round — gated by
    /// [`CONSUMABLES_PER_ROUND`] (1). Reset by `reset_fighters_for_next_round`.
    /// [Phase 4.3]
    pub consumables_used: u32,
    /// When this fighter's continuous gear damage (`loadout.continuous_damage`) next
    /// ticks. `None` until the round is live; cleared at every round reset so a new
    /// round's first tick lands one interval after it starts.
    pub continuous_next_tick_at: Option<Instant>,
    /// Fractional damage the continuous tick owes but has not yet taken. Health is
    /// integral, and an Ebony Mail tick is 1.88: rounding each one would bill 2.0
    /// (10/s against the 9.4/s the item text states), truncating would bill 1.0.
    pub continuous_carry: f32,
    /// The consumable item UUID this fighter has equipped, as declared by its own
    /// `EquipAbilitiesAndConsumables` (56) upload (`{4:String consumableUuid ·
    /// 5:Int charges}`). It is the ONLY source of the UUID the server must echo in
    /// `PerformConsumeConsumable` (64) when the client requests a potion — the
    /// request (63) itself carries no payload. `None` until the client uploads its
    /// loadout. [Phase 4.3 wire trigger]
    pub equipped_consumable: Option<String>,
    /// An in-flight potion. Retail spreads a restoration over
    /// `_restorationDuration` (2.5 s for every shipped tier) rather than
    /// applying it in one lump, so this is drained by `apply_regen_tick`
    /// instead of being added on the spot — the tick already owns pool
    /// changes and already emits the stats update, so nothing new has to
    /// learn how to talk to the client.
    pub pending_restore: Option<PendingRestore>,
    /// How long the CURRENT paralysis lasts (the casting rank's shipped `_duration`);
    /// read by `resolve::reconcile_paralysis`. [Phase 3.9]
    pub paralyze_secs: f32,
    /// When this fighter last entered `Staggered` or `Paralyzed`, until
    /// [`super::interrupts::process_interrupts`] has cancelled whatever that
    /// interrupted. Set at the single state seam ([`Self::force_actor_state`]) and by
    /// a re-stagger, so no stagger or paralysis path can forget it.
    pub interrupt_pending: Option<Instant>,
    /// Casts whose interruptible phase is still running: a spell's `AbilityChannel`
    /// wind-up, a channelled spell, or a weapon maneuver up to `OnManeuverEnded`.
    /// See [`Execution`].
    pub executions: Vec<Execution>,
}

/// One cast in its interruptible phase.
///
/// The client ends a spell's channel or wind-up, and a maneuver, when the actor leaves
/// `Channeling` / `Maneuver` for `Staggered` or `Paralyzed`
/// (`ChannelingState$$EndChanneling@0x1a52578`, `ActorManeuverState$$OnExit@0x1d556b0`):
/// `RequestInterruptAbility` → `AbilityExecutor$$ForceStop@0x1e9ba28` runs every step's
/// cleanup and drops the rest, so no damage step runs and the cooldown starts at the
/// interrupt (`AbilityExecutor$$Interrupt@0x1e9b94c` → `EndExecution`). In PvP the
/// server drives that with op59 `InterruptAbility` (combat-spec 06 §1.8, 07 §5.5).
#[derive(Debug, Clone)]
pub struct Execution {
    pub ability_uuid: String,
    /// The instant the cast was accepted (op38). Pending impacts and channels carry
    /// the same instant as `cast_at`, which is how they are matched to this record.
    pub started_at: Instant,
    /// End of the interruptible phase: the channel's end, or `OnManeuverEnded`.
    pub until: Instant,
    /// The rank's full cooldown, which starts when the execution ends.
    pub cooldown: std::time::Duration,
    /// A weapon maneuver (the caster is in `Maneuver`), not a spell.
    pub is_maneuver: bool,
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
    /// What FRACTION of each incoming hit this pool may eat, before its `remaining`
    /// budget is considered. **1.0 for Ward / Absorb / Dodge** — they swallow a hit
    /// whole until the pool is exhausted, which is what they did before this field
    /// existed, so the default is behaviour-preserving.
    ///
    /// The storm-armor shields are the reason it exists: they ship
    /// `_damageAbsorptionPercent` = **0.50 at every rank**, so they absorb HALF of each
    /// hit until their 116-158 pool drains. Treating them as full absorbers made them
    /// twice as strong per hit and drained them twice as fast.
    pub absorb_fraction: f32,
    /// Restrict this pool to ELEMENTAL damage (Fire/Frost/Shock/Poison).
    ///
    /// **True for Ward only.** The shipped description is explicit —
    /// `Ability.Spell.Ward.Description`: *"increases Armor Rating by {0} and negates
    /// up to {1} elemental damage, plus any excess damage from the attack that
    /// destroys it."* Ward's physical protection is the Armor Rating term (the
    /// `transient_resistances` this spell also pushes), NOT the pool. Letting the pool
    /// eat Slashing / Cleaving / Bashing spent the elemental budget on weapon swings
    /// and left nothing for the spell it was cast to stop — the defect behind the
    /// production event where a 120.54 Ward pool still let 112.37 poison through:
    /// the physical hits had already drained it.
    ///
    /// False for Absorb / Dodge / the storm-armor shields, which are not
    /// element-scoped — so the default is behaviour-preserving.
    pub elemental_only: bool,
    /// When this pool is exhausted BY a hit, also negate the remainder of that same
    /// hit — within the component set this pool was eligible for.
    ///
    /// **True for Ward only** — the "plus any excess damage from the attack that
    /// destroys it" clause above. Without it the breaking hit's overflow leaks, making
    /// Ward strictly weaker than its own description at precisely the moment it is
    /// supposed to pay off. Scoped to the eligible components so an elemental-only
    /// Ward cannot swallow a physical component it was never allowed to touch.
    pub consumes_overflow: bool,
    /// Granted ONCE if this pool actually absorbs damage — the dodge maneuvers'
    /// reward for a dodge that connected. `(health, magicka, cooldown_seconds)`
    /// from `_maximumHealthRestored` (Adrenaline Dodge, 43.5),
    /// `_maximumMagickaRestored` (Renewing Dodge, 338) and
    /// `_maximumCooldownReduction` (Focusing Dodge, 4.0 s).
    ///
    /// All three were read by nobody, so those three maneuvers were identical to a
    /// plain Dodging Strike — they cost their stamina and gave only the dodge.
    ///
    /// These are authored as CAPS. The shipped data has no proximity curve ("the
    /// closer you are to being hit, the more you get"), so the cap is granted whole
    /// on a dodge that connects: it is the only number the data actually contains,
    /// and awarding nothing was the bug.
    pub on_absorb_restore: (f32, f32, f32),
    /// Damage types this pool does NOT absorb — `_vulnerableDamageTypes`.
    ///
    /// Blizzard Armor ships `[4 Fire]`: the ice shield is weak to fire. The shipped
    /// data names the TYPE and no magnitude, so the faithful reading with nothing
    /// invented is that the shield simply does not stop it — fire passes straight
    /// through a Blizzard Armor instead of being halved by it. Inventing a
    /// "takes 1.5x fire" multiplier would be making up a number the game never
    /// authored.
    pub bypass_types: &'static [i32],
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
pub const BASE_STAGGER_DURATION_SECS: f32 = PVP_BASE_STAGGER_DURATION;

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
        // Arena triples HEALTH only (`ARENA_HEALTH_MULTIPLIER`), including the
        // primary Fortify Health enchantments. Stamina/Magicka are not multiplied.
        let max_health = ((health_for_level(loadout.level) as f32 + loadout.max_health_bonus)
            * ARENA_HEALTH_MULTIPLIER as f32)
            .round()
            .max(1.0) as u32;
        // A real character's pools come from its own attribute spend; a bot or the
        // starter loadout has no spend to read and keeps the level approximation.
        let (max_stamina, max_magicka) = if loadout.has_character {
            (
                pool_for_points(loadout.stamina_points),
                pool_for_points(loadout.magicka_points),
            )
        } else {
            (pool_for_level(loadout.level), pool_for_level(loadout.level))
        };
        Fighter {
            slot,
            net_object_id,
            firewall_until: None,
            firewall_damage: 0.0,
            firewall_self_pct: 0.0,
            echo_until: None,
            echo_bonus: 0.0,
            echo_delay: 0.0,
            weakness_rating: 0.0,
            reflect_until: None,
            reflect_remaining: 0.0,
            magicka_surge_until: None,
            magicka_surge_bonus: 0.0,
            no_magicka_regen_until: None,
            reckless_fury_until: None,
            reckless_fury_bonus: 0.0,
            maneuver_state_until: None,
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
            ravaged_stamina: 0,
            ravaged_magicka: 0,
            ravaged_health: 0,
            effects: Vec::new(),
            // Construction, not a transition — nothing to tell a client that has no
            // avatar yet, so the field is set directly rather than via the mutator.
            actor_state: ActorStateType::Idle,
            pending_state_changes: Vec::new(),
            scheduled_states: Vec::new(),
            state_history: VecDeque::new(),
            transitions_total: 0,
            channel_pose: None,
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
            bot_cast_counts: std::collections::HashMap::new(),
            bot_last_cast: None,
            combo_count: 0,
            last_combo_side: ActiveSide::None,
            last_swing_side: ActiveSide::None,
            charge_began_at: None,
            recovery_began_at: None,
            last_input_x: None,
            last_input_y: None,
            last_input_at: None,
            pending_manual_attack: None,
            active_manual_attack: None,
            last_client_charge: None,
            last_input_block_zone: None,
            damage_history: HashMap::new(),
            can_be_paralyzed: true, // players can be paralysed (vs boss innate immunity)
            negation_pools: Vec::new(),
            transient_resistances: Vec::new(),
            transient_all_resistance: Vec::new(),
            staggered_until: None,
            announced_statuses: Vec::new(),
            status_timers: Vec::new(),
            announced_blocking: false,
            consumables_used: 0,
            continuous_next_tick_at: None,
            continuous_carry: 0.0,
            equipped_consumable: None,
            pending_restore: None,
            paralyze_secs: paralyze_duration_secs(1),
            interrupt_pending: None,
            executions: Vec::new(),
        }
    }

    /// Is a weapon maneuver still executing (before its `OnManeuverEnded`)? While it
    /// is, the caster is in the client's `Maneuver` state, which admits no Blocking,
    /// Charging or attack (`ActorManeuverState$$CanTransitionTo@0x1d55814`).
    pub fn maneuver_active(&self, now: Instant) -> bool {
        self.executions.iter().any(|e| e.is_maneuver && now < e.until)
    }

    /// Is an op53 cast pose still standing, i.e. is the client showing this actor in
    /// `Channeling`? `Actor$$CanCast@0x1c58f54` lets a cast through from Channeling.
    pub fn in_channel_pose(&self, now: Instant) -> bool {
        matches!(self.channel_pose, Some((until, at)) if at == self.transitions_total && now < until)
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
        self.force_actor_state(next, now);
    }

    /// Queue a transition even when the logical state already has this value.
    /// Specialised packets such as op53 can put the client in a pose without changing
    /// `actor_state`; round boundaries use this to reassert Idle authoritatively.
    pub fn force_actor_state(&mut self, next: ActorStateType, now: Instant) {
        let from = self.actor_state;
        let time_in_previous = self.time_in_state(now);
        // Entering Staggered or Paralyzed interrupts a channel, a spell wind-up or a
        // maneuver (see [`Execution`]). Keep the earliest unprocessed instant.
        if matches!(next, ActorStateType::Staggered | ActorStateType::Paralyzed) {
            self.interrupt_pending.get_or_insert(now);
        }
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

    /// Record a state carried by a specialised state-change message without changing
    /// the resolver's logical input-gating state.
    ///
    /// Maneuvers are announced by op58 rather than the generic actor-state outbox.
    /// The client still requires op58's `PvpPlayerStateHistory`, and retail always has
    /// the announced `Maneuver` state as its newest entry. Recording it here keeps the
    /// shared history/index stream monotonic while avoiding a second, incorrect gmid39
    /// for the same transition. It also lets a shield bash remain logically Blocking
    /// during its first phase while op58 plays the compound block-then-strike clip.
    pub fn record_presentational_state(&mut self, next: ActorStateType) -> Vec<u8> {
        if self.state_history.len() == STATE_HISTORY_MAX {
            self.state_history.pop_front();
        }
        self.state_history.push_back(next);
        self.transitions_total = self.transitions_total.saturating_add(1);
        self.packed_state_history()
    }

    /// Record the `Channeling` state op53 announces and remember when the channel ends.
    /// Returns the history ring for op53's propId 7, whose newest entry is Channeling.
    ///
    /// The logical `actor_state` is left alone (as for op58), but the client is not:
    /// op53 puts the caster in `PlayerChannelingState` on both clients, and the
    /// opponent's copy never leaves it on its own — `PvpOpponentActor$$TryChangeState@0x1964cc8`
    /// refuses every local change, and the cast animation ends only in
    /// `ChannelingState$$EndChanneling@0x1a52578`, run from the state's exit. On the
    /// caster's own client `AbilityChannel$$Cleanup@0x1e8faa4` goes to Idle after the
    /// step's `_duration`. So the server owes both viewers a state change at that
    /// instant ([`Self::reconcile_channel_pose`]; combat-spec 12 §5.2, 12-D2).
    pub fn begin_channel_pose(&mut self, channel_ends_at: Instant) -> Vec<u8> {
        let blob = self.record_presentational_state(ActorStateType::Channeling);
        self.channel_pose = Some((channel_ends_at, self.transitions_total));
        blob
    }

    /// End an op53 cast pose that is due. Re-asserts the logical state (Idle in
    /// practice) so the drain sends a 39 to both viewers — but only when nothing has
    /// entered a state since op53. Any later transition (a swing, a stagger, op58,
    /// death, a round-end Idle) already took the client out of Channeling, and a
    /// second message would be a spurious one.
    pub fn reconcile_channel_pose(&mut self, now: Instant) {
        let Some((until, recorded_at)) = self.channel_pose else {
            return;
        };
        if self.transitions_total != recorded_at {
            self.channel_pose = None;
            return;
        }
        if now < until {
            return;
        }
        self.channel_pose = None;
        let logical = self.actor_state;
        self.force_actor_state(logical, now);
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
        self.reconcile_channel_pose(now);
    }

    /// Seconds spent in the current actor state — the `timeInState` float the
    /// `PlayerStateChange` family carries at propId 8.
    pub fn time_in_state(&self, now: Instant) -> f32 {
        now.saturating_duration_since(self.state_entered).as_secs_f32()
    }

    /// This fighter's live **Block Rating** `R` while guarding, before piercing:
    /// the blocking item's `R0`, times `(1 + OptimalBlockBoost)` of THAT item on an
    /// optimal block, plus the flat Block Reduction enchants.
    ///
    /// `Actor$$ResolveBlocking@0x1c54284` (0x1c5433c/0x1c54358) computes
    /// `R = R0 + R0 × boost`, i.e. `×(1 + boost)`: ×2 for the boost of 1.0 that every
    /// shield and 363 of 370 weapons ship, ×1.5 / ×1 / ×3 for the other seven weapons.
    /// The additive `BlockRatingMethod` sources come after the boost, so they are not
    /// doubled.
    pub fn block_rating(&self, optimal: bool) -> f32 {
        let r0 = self.loadout.block_rating;
        let boosted = if optimal { r0 * (1.0 + self.loadout.blocking_item_boost()) } else { r0 };
        boosted + self.loadout.block_rating_bonus
    }

    /// True iff this fighter is currently staggered. [Phase 3.13]
    pub fn is_staggered(&self, now: Instant) -> bool {
        matches!(self.staggered_until, Some(t) if now < t)
    }

    /// Frozen is an elemental slow, not a stagger/paralysis input lock. The client
    /// animates it from op51; the resolver uses this to scale weapon charge/cadence.
    pub fn is_frozen(&self, now: Instant) -> bool {
        self.effects
            .iter()
            .any(|effect| effect.effect == StatusEffectType::Frozen && now < effect.expires_at)
    }

    /// Enter the staggered state for `CombatParameters.baseStaggerDuration`.
    pub fn apply_stagger(&mut self, now: Instant) {
        self.apply_stagger_for(now, BASE_STAGGER_DURATION_SECS);
    }

    /// Enter the staggered state for an explicit duration.
    ///
    /// Exists because some abilities ship their OWN `_stunDuration` — IceSpike 1.20 s,
    /// StaggeringBash, Guardbreaker — and that field was read by nothing, so every one
    /// of them produced the generic `baseStaggerDuration` instead of its own.
    ///
    /// Stagger is used as the vehicle deliberately. `StatusEffectType` has no `Stun`
    /// member: the wire value for a distinct stun status is not pinned by any capture
    /// we hold, and inventing a status id risks the client dropping the frame silently
    /// (`FindStateTypeByID` returns null and the effect evaporates). A stun and a
    /// stagger do the same observable thing here — inputs locked, guard dropped, combo
    /// broken — so this reuses the capture-validated stagger path with the ability's
    /// real duration. If a real Stun id turns up in the dump, this is the one place to
    /// change.
    /// Is this fighter under `StaggeredWeakness` right now? It lives exactly as long
    /// as the Staggered that produced it — the status ships no duration of its own.
    pub fn has_staggered_weakness(&self, now: Instant) -> bool {
        self.weakness_rating > 0.0 && self.is_staggered(now)
    }

    ///
    /// Returns whether the stagger LANDED. The client refuses a Staggered add when
    /// `!CanBeStaggered` — `Actor$$get_CanBeStaggered@0x1c5b9f4` is
    /// `!HasStatus(RecklessFury 11) && !HasStatus(Paralyzed 9)` — and on a dead actor
    /// (`Actor$$AddStatusEffectInternal@0x1c5700c`). A refused stagger has no status,
    /// so callers must send no op51 and fire no `CausedStagger` hook for it.
    pub fn apply_stagger_for(&mut self, now: Instant, secs: f32) -> bool {
        // Reckless Fury cannot be stunned. This is the guard whose absence let the
        // production stun in match fffe01ca land 2 s into a 5 s Fury.
        //
        // Paralysis cannot be staggered either: `ActorParalyzedState$$CanTransitionTo
        // @0x1fd4c48` admits only Dead and EnemyNonLethal, so a stagger here used to
        // overwrite the Paralyzed state and end the lock early.
        if self.has_reckless_fury(now) || self.is_paralyzed() || self.is_dead() {
            return false;
        }
        // A FRESH stagger starts without a weakness. `StaggeredWeakness` lives exactly
        // as long as the Staggered that produced it (`Actor$$ReactToStatusRemoved
        // @0x1c57850`), so a rating left over from an earlier stagger must not ride
        // this one.
        if !self.is_staggered(now) {
            self.weakness_rating = 0.0;
        }
        self.staggered_until =
            Some(now + std::time::Duration::from_secs_f32(secs.max(0.05)));
        // A re-stagger does not change the state, so the seam in `force_actor_state`
        // does not see it; it still interrupts a maneuver started from the first
        // stagger (Recovery Strikes, a dodge).
        self.interrupt_pending.get_or_insert(now);
        self.set_actor_state(ActorStateType::Staggered, now);
        // A stagger overrides whatever swing was in flight: drop its queued
        // follow-through/recovery/idle, which would otherwise fire mid-stagger and
        // return the actor to Idle early.
        self.clear_scheduled_states();
        // A staggered fighter's guard drops and its combo chain breaks.
        self.blocking_until = None;
        self.block_raised_at = None;
        self.reset_combo();
        true
    }

    /// Clear an expired stagger, returning true when the fighter just recovered.
    pub fn reconcile_stagger(&mut self, now: Instant) -> bool {
        if let Some(t) = self.staggered_until {
            if now >= t {
                self.staggered_until = None;
                // The weakness ends with the stagger that produced it.
                self.weakness_rating = 0.0;
                if self.actor_state == ActorStateType::Staggered {
                    self.set_actor_state(ActorStateType::Idle, now);
                }
                return true;
            }
        }
        false
    }

    /// The statuses this fighter is currently under, restricted to the ones whose
    /// lifetime the engine actually models here.
    ///
    /// `effects` covers the elemental conditions and Blind; stagger, paralysis,
    /// Fury, Wall of Fire and StaggeredWeakness live in their own fields; Ward,
    /// Absorb, the storm armors and Dodging in `negation_pools`; 60-63 and
    /// FlashFreeze's Frozen in `status_timers`. Blocking is synced separately
    /// ([`Self::sync_blocking_status`]).
    pub fn tracked_statuses(&self, now: Instant) -> Vec<StatusEffectType> {
        let mut v: Vec<StatusEffectType> =
            self.effects.iter().filter(|e| now < e.expires_at).map(|e| e.effect).collect();
        if self.is_staggered(now) {
            v.push(StatusEffectType::Staggered);
        }
        if self.is_paralyzed() {
            v.push(StatusEffectType::Paralyzed);
        }
        // A live dodge window IS a tracked status, so the diff below emits its
        // remove when the window closes — whether it closed because the second
        // lapsed or because the dodge actually ate a hit (`apply_negation_pools`
        // drops a drained pool).
        //
        // Retail sends that remove: of 405 captured `Dodging` (12) op51 frames,
        // 204 are applies and 201 are removes. We sent the apply and never the
        // remove, so the client's dodge indicator had nothing to clear on.
        //
        // Ward, Absorb and the storm armors share `negation_pools`; they are mapped
        // to their own statuses just below.
        if self.negation_pools.iter().any(|p| {
            p.source == DamageNegationSource::Dodge && p.remaining > 0.0 && now < p.expires_at
        }) {
            v.push(StatusEffectType::Dodging);
        }
        // Every other status we announce, so each apply gets its remove. The PvP
        // client ends a status ONLY on the server's op51 remove
        // (`PvpAvatar$$OnCombatMessage@0x1792aa4` -> `ForceRemoveStatusEffect`); its
        // own timer never fires (`PvpPlayerActor$$RemoveStatusEffect@0x1a32aa8`).
        //
        // The three pool-backed buffs end when their pool empties OR times out,
        // as the client's step does: `AbilityDoWard$$Update@0x1e9aea0`,
        // `AbilityDoAbsorb$$Update@0x1e91104` and `AbilityDoStormArmor$$Update
        // @0x1e9a3ac` complete there, and each `Cleanup` removes its status.
        // Ward and the storm armors share `DamageNegationSource::Ward`; Ward is the
        // only `elemental_only` pool (see `NegationPool::elemental_only`).
        for p in &self.negation_pools {
            if p.remaining <= 0.0 || now >= p.expires_at {
                continue;
            }
            match p.source {
                DamageNegationSource::Absorb => v.push(StatusEffectType::Absorb),
                DamageNegationSource::Ward if p.elemental_only => v.push(StatusEffectType::Ward),
                DamageNegationSource::Ward => v.push(StatusEffectType::ElementalStormArmor),
                _ => {}
            }
        }
        // Fury's status is removed at the buff step's cleanup
        // (`AbilityDoRecklessFury$$Cleanup@0x1e980b8`). The client gates its guard
        // prediction on `!HasStatus(11)` (`Actor$$get_CanBlock@0x1c5b9d4`), so a
        // Fury that is never removed leaves that player unable to raise a guard
        // for the rest of the round.
        if self.has_reckless_fury(now) {
            v.push(StatusEffectType::RecklessFury);
        }
        // Wall of Fire: `AbilityDoFirewall$$Cleanup@0x1e9463c` removes status 13.
        if self.firewall_until.is_some_and(|t| now < t) {
            v.push(StatusEffectType::Firewall);
        }
        // Removed with the Staggered that produced it (`ReactToStatusRemoved`).
        if self.has_staggered_weakness(now) {
            v.push(StatusEffectType::StaggeredWeakness);
        }
        v.extend(self.status_timers.iter().filter(|(_, t)| now < *t).map(|(st, _)| *st));
        v.sort_by_key(|s| *s as u16);
        v.dedup();
        v
    }

    /// Does this fighter hold an Absorb shield right now? Ice Spike's stagger, the
    /// Blind and Paralyze procs, and the Staggering Bash / Guardbreaker stun all skip
    /// a target with status Absorb (17): `AbilityApplyIceSpikeDamage$$OnDamage
    /// @0x1e8ea14`, `AbilityApplyPoisonSpellDamage$$OnDamage@0x1e8ebb0`,
    /// `AbilityDoStaggeringBash$$ApplyAdditionalEffects@0x1e9a038`,
    /// `AbilityDoGuardbreaker$$ApplyAdditionalEffects@0x1e956bc`.
    pub fn has_absorb(&self, now: Instant) -> bool {
        self.negation_pools.iter().any(|p| {
            p.source == DamageNegationSource::Absorb && p.remaining > 0.0 && now < p.expires_at
        })
    }

    /// Is the guard up at `now`? This is when the client holds the `Blocking` status
    /// (`PlayerBlockingState$$OnLateUpdate@0x1d728d0` adds it once startup elapses,
    /// which is immediate for a PvP opponent's `InstantShieldBlock`; `OnExit
    /// @0x1d72f24` removes it). The bash guard window counts too
    /// (`AbilityDoShieldBash$$Update@0x1e98a78`).
    pub fn guard_up(&self, now: Instant) -> bool {
        matches!(self.blocking_until, Some(t) if now < t)
    }

    /// `Some(true)` when the guard just came up and the clients have not been told,
    /// `Some(false)` when it just came down, `None` when nothing changed. Records the
    /// answer, so each edge is reported once.
    ///
    /// Retail sends Blocking (1) both ways (736 applies and 756 removes in s615+s616),
    /// and the PvP client depends on it: `PvpPlayerActor$$AddStatusEffect@0x1a32aa0`
    /// is a no-op, so the status exists only if the server sends it. The client's HUD
    /// combat state (`PlayerActor$$get_CombatState@0x18698d4`) and its "late block"
    /// feedback (`CombatHUDMenu$$ShowBlockingFeedback@0x1989b3c`) read the status,
    /// not the actor state.
    pub fn sync_blocking_status(&mut self, now: Instant) -> Option<bool> {
        let up = self.guard_up(now);
        if up == self.announced_blocking {
            return None;
        }
        self.announced_blocking = up;
        Some(up)
    }

    /// Statuses that have LAPSED since the last call — the ones the client still
    /// believes are active and needs an op51 remove for.
    ///
    /// Also records what is active now, so the next call can diff against it.
    /// Call once per tick, per fighter.
    pub fn drain_lapsed_statuses(&mut self, now: Instant) -> Vec<StatusEffectType> {
        let active = self.tracked_statuses(now);
        let lapsed: Vec<StatusEffectType> =
            self.announced_statuses.iter().copied().filter(|s| !active.contains(s)).collect();
        self.announced_statuses = active;
        lapsed
    }

    /// Record that an explicit op51 remove has already been sent for `status`.
    /// Cures use this so the next timer diff does not announce the same removal a
    /// second time.
    pub fn acknowledge_status_removed(&mut self, status: StatusEffectType) {
        self.announced_statuses.retain(|announced| *announced != status);
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

    /// Reset the combo chain (`combo_count` → 0, `last_combo_side` → None).
    /// `Actor$$ResetCombo@0x1c5bf5c`; the call sites are combat-spec 02 §4.3 R1-R11.
    pub fn reset_combo(&mut self) {
        self.combo_count = 0;
        self.last_combo_side = ActiveSide::None;
    }

    /// A new normal (Left/Right) swing begins. The count is NOT advanced here: it
    /// counts connected hits ([`Self::increment_combo`]). What this decides is
    /// whether the chain survives into the new swing. A charge that alternates sides
    /// keeps it; a repeated side, or no previous side, starts a fresh chain
    /// (`PlayerActorState$$ChangeStateIfRequired@0x1d5c444`, 02 R2).
    ///
    /// Returns the count the coming hit will read.
    pub fn begin_combo_swing(&mut self, side: ActiveSide) -> u32 {
        let alternates = matches!(
            (self.last_combo_side, side),
            (ActiveSide::Left, ActiveSide::Right) | (ActiveSide::Right, ActiveSide::Left)
        );
        if !alternates {
            self.combo_count = 0;
        }
        self.last_combo_side = side;
        self.combo_count
    }

    /// End the chain if Recovery has already run out (02 R1, X5). The actor goes Idle
    /// `recoveryTime` after its last Recovery began, and Idle's `OnEnter` resets the
    /// combo (`PlayerIdleState$$OnEnter@0x1d75368`). `next_action_began` is when the
    /// action that would read the chain began: a charge's start, a maneuver's cast,
    /// or `now` when the question is just "is it still live". A charge begun inside
    /// Recovery keeps the chain however long it is then held.
    pub fn expire_combo_chain(&mut self, next_action_began: Instant) {
        let Some(began) = self.recovery_began_at else {
            return;
        };
        let expiry =
            std::time::Duration::from_secs_f32(self.loadout.charge_params().recovery_time.max(0.0));
        if next_action_began.saturating_duration_since(began) >= expiry {
            self.reset_combo();
            self.recovery_began_at = None;
        }
    }

    /// A weapon hit (source Attack or WeaponManeuver) connected, negated or not:
    /// `CombatManager$$ApplyDamage@0x1bd2770` calls `IncrementCombo` after
    /// `ReceiveDamage` (02 §4.1, X8).
    pub fn increment_combo(&mut self) {
        self.combo_count = self.combo_count.saturating_add(1);
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

    /// The current OPTIMAL/LATE (low) block phase for `now`:
    /// - OPTIMAL iff the guard has been up for < `BLOCK_OPTIMAL_TIME_SECS` **and** it
    ///   was raised at least `OPTIMAL_BLOCK_RECOVERY_SECS` after the previous release
    ///   (or there was none — the first guard of the match is always eligible);
    /// - LATE (a low block) otherwise.
    ///
    /// **Eligibility is latched at the press, not re-checked at the hit.**
    /// `PlayerActorState$$ChangeToBlockingState@0x1d5beec` stores
    /// `OptimalBlockAllowed = !_optimalBlockTimeCooldownEnabled` (`@0x1c4ef1c`) into
    /// the state's parameters when the guard goes up, and
    /// `PlayerBlockingState$$get_IsOptimalBlocking@0x1d7288c` reads only that latch
    /// and the window. So a guard raised 0.5 s after a release stays LOW for its whole
    /// life, even when the hit lands after the 0.8 s cooldown has run out. The latch is
    /// derived from the raise and release instants, which is the same fact.
    ///
    /// Returns `None` when the guard is not up.
    pub fn block_phase(&self, now: Instant) -> Option<BlockPhase> {
        let raised = self.block_raised_at?;
        // Guard must still be up.
        if !matches!(self.blocking_until, Some(until) if now < until) {
            return None;
        }
        let held_secs = now.saturating_duration_since(raised).as_secs_f32();
        if held_secs >= BLOCK_OPTIMAL_TIME_SECS {
            return Some(BlockPhase::Late);
        }
        let optimal_allowed = self.last_block_dropped_at.is_none_or(|dropped| {
            raised.saturating_duration_since(dropped).as_secs_f32() >= OPTIMAL_BLOCK_RECOVERY_SECS
        });
        Some(if optimal_allowed { BlockPhase::Optimal } else { BlockPhase::Late })
    }

    /// The `ReceiveDamage` bit 3 (`wasOptimalBlocking`) for a frame addressed to this
    /// fighter. It is DEFENDER STATE, read for every source — DoT ticks included —
    /// not a verdict on the hit: `CombatManager$$ApplyDamage@0x1bd2770` passes
    /// `target.IsOptimalBlocking` to `ReceiveDamage` (0x1bd2a24–0x1bd2a58).
    pub fn optimal_block_flag(&self, now: Instant) -> u8 {
        let optimal = self.actor_state() == ActorStateType::Blocking
            && self.block_phase(now) == Some(BlockPhase::Optimal);
        if optimal {
            crate::arena::combat::damage::flags::WAS_OPTIMAL_BLOCKING
        } else {
            0
        }
    }

    /// Return the sum of transient Resist-Elements resistances for `ty` (non-expired
    /// only). Called by the damage pipeline to add to loadout resistances. [§4.3]
    pub fn transient_resistance_against(&self, ty: DamageType, now: Instant) -> f32 {
        // The all-types band (Willpower, Indomitable Smash) applies to every type, so
        // it is summed in here rather than at each call site.
        let all: f32 = self
            .transient_all_resistance
            .iter()
            .filter(|(_, exp)| now < *exp)
            .map(|(v, _)| *v)
            .sum();
        // COMBAT FOCUS. `CombatFocusPerk$$GetResistanceBonus@0x1A5AA60` returns 0 for
        // `DamageType.None`, otherwise `_bonusValue` iff the actor is in the
        // `Maneuver` state OR has the Reckless Fury status (11). Never while casting a
        // spell (spells run in `Channeling`). It is ONE additive source, so
        // overlapping maneuvers do not stack it.
        let focus = if ty != DamageType::None && self.combat_focus_active(now) {
            self.loadout.perks.combat_focus
        } else {
            0.0
        };
        let all = all + focus;
        let per_type: f32 = self
            .transient_resistances
            .iter()
            .filter(|(t, _, exp)| *t == ty && now < *exp)
            .map(|(_, v, _)| *v)
            .sum();
        all + per_type
    }

    /// Prune expired transient resistances.
    pub fn prune_transient_resistances(&mut self, now: Instant) {
        self.transient_resistances.retain(|(_, _, exp)| now < *exp);
        self.transient_all_resistance.retain(|(_, exp)| now < *exp);
    }

    pub fn is_dead(&self) -> bool {
        self.health == 0
    }

    /// Is Reckless Fury active right now?
    pub fn has_reckless_fury(&self, now: Instant) -> bool {
        self.reckless_fury_until.is_some_and(|t| now < t)
    }

    /// Combat Focus's condition: in the Maneuver state, or Reckless Fury is up.
    pub fn combat_focus_active(&self, now: Instant) -> bool {
        self.maneuver_state_until.is_some_and(|t| now < t) || self.has_reckless_fury(now)
    }

    /// Apply `amount` raw damage to health, clamped at 0, and bump the stats seq.
    ///
    /// While Reckless Fury is up the fighter CANNOT DIE: health floors at 1 instead
    /// of 0. Use [`Fighter::take_damage_at`] to get that protection — this entry
    /// point has no clock and so cannot check the window.
    pub fn take_damage(&mut self, amount: u32) {
        self.health = self.health.saturating_sub(amount);
        self.stats_seq = self.stats_seq.wrapping_add(1);
    }

    /// `take_damage` with Reckless Fury's death prevention applied.
    ///
    /// Fury's description promises the wielder cannot be killed for its duration; the
    /// ability had no persistent state at all, so this never held. Floors at 1 HP
    /// rather than 0 — the fighter survives the window and dies normally afterwards
    /// if the damage keeps coming.
    pub fn take_damage_at(&mut self, amount: u32, now: Instant) {
        if self.has_reckless_fury(now) {
            let floor = 1;
            self.health = self.health.saturating_sub(amount).max(floor.min(self.health));
            self.stats_seq = self.stats_seq.wrapping_add(1);
            return;
        }
        self.take_damage(amount);
    }

    /// Apply the **non-health** damage components of a hit to their pools:
    /// `DamageType::Stamina` drains stamina and `DamageType::Magicka` drains
    /// magicka, both clamped at 0.
    ///
    /// These are the *mirrored drains* the shipped `CombatParameters` define —
    /// `frostDamageToStaminaDamage = 1` (Frost → Stamina) and
    /// `shockDamageToMagickaDamage = 1` (Shock → Magicka). `damage::mirrored_drain`
    /// has always put them on the wire, and the retail capture agrees they belong
    /// there (s615 #4394011: `Frost 13.19 + Stamina 13.19` in one `ReceiveDamage`) —
    /// but **nothing ever subtracted them**. `emit_damage` summed only the
    /// health-typed components and called `take_damage`, so a Frostbite landing on a
    /// full stamina bar left it full and the frame's own packed stats said so. That
    /// is the whole distinctive effect of a frost build doing nothing.
    ///
    /// Returns `(stamina_drained, magicka_drained)` for logging.
    pub fn drain_mirrored_pools(&mut self, components: &[(DamageType, f32)]) -> (u32, u32) {
        let take = |ty: DamageType| -> u32 {
            components
                .iter()
                .filter(|(t, _)| *t == ty)
                .map(|(_, v)| *v)
                .sum::<f32>()
                .round()
                .max(0.0) as u32
        };
        let s = take(DamageType::Stamina);
        let m = take(DamageType::Magicka);
        if s == 0 && m == 0 {
            return (0, 0);
        }
        let drained_s = s.min(self.stamina);
        let drained_m = m.min(self.magicka);
        self.stamina -= drained_s;
        self.magicka -= drained_m;
        self.stats_seq = self.stats_seq.wrapping_add(1);
        (drained_s, drained_m)
    }

    /// Take `attacker`'s Ravage off this fighter's MAXIMUM pools, scaled by `factor`.
    ///
    /// Returns `(stamina, magicka)` actually removed, for the log. Current pool is
    /// clamped down with the ceiling: a fighter sitting on a full bar loses the
    /// stamina, it does not sit above its own maximum.
    ///
    /// `factor` is the hit's PHYSICAL block factor — ravage rides the weapon swing, so
    /// an optimal block (physical x0) negates it outright and a late block reduces it
    /// in proportion. A dodged swing never reaches this function at all, because no
    /// hit resolves. **This scaling is authored, not measured** — ravage is absent from
    /// the capture corpus entirely (no op50 component, no status effect), so no capture
    /// can settle it; see `docs/arena-ravage.md`.
    pub fn apply_ravage(&mut self, ravage: &[(DamageType, f32)], factor: f32) -> (u32, u32, u32) {
        if ravage.is_empty() || factor <= 0.0 {
            return (0, 0, 0);
        }
        let (mut took_s, mut took_m, mut took_h) = (0_u32, 0_u32, 0_u32);
        for (ty, amount) in ravage {
            let cut = (amount * factor).round().max(0.0) as u32;
            if cut == 0 {
                continue;
            }
            match ty {
                DamageType::Stamina => {
                    // Never below zero, and never more than is left to take.
                    let cut = cut.min(self.max_stamina);
                    self.max_stamina -= cut;
                    self.ravaged_stamina += cut;
                    self.stamina = self.stamina.min(self.max_stamina);
                    took_s += cut;
                }
                DamageType::Magicka => {
                    let cut = cut.min(self.max_magicka);
                    self.max_magicka -= cut;
                    self.ravaged_magicka += cut;
                    self.magicka = self.magicka.min(self.max_magicka);
                    took_m += cut;
                }
                DamageType::Health => {
                    // The arena's x3 health cheat multiplies the POOL, not the effects
                    // that act on it, so the cut is flat — the same 31.66/41.5/52.66 a
                    // stamina ravage takes, against a tripled ceiling.
                    //
                    // Floored at 1: a maximum of zero would make `wire_fraction`
                    // divide by zero and read as dead without anyone landing a blow.
                    // Ravage empties the ceiling, it does not execute.
                    let cut = cut.min(self.max_health.saturating_sub(1));
                    self.max_health -= cut;
                    self.ravaged_health += cut;
                    self.health = self.health.min(self.max_health);
                    took_h += cut;
                }
                _ => {}
            }
        }
        if took_s > 0 || took_m > 0 || took_h > 0 {
            self.stats_seq = self.stats_seq.wrapping_add(1);
        }
        (took_s, took_m, took_h)
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
    pub fn weakness_rating_against(&self, ty: DamageType, now: Instant) -> f32 {
        let gear: f32 = self
            .loadout
            .weaknesses
            .iter()
            .filter(|(t, _)| *t == ty)
            .map(|(_, v)| *v)
            .sum();
        // `StaggeredWeakness` (Powerful Block) is TYPE-AGNOSTIC: the client's
        // `GetWeakness(DamageType)` is two instructions and never reads its argument,
        // and the capture agrees — one identical delta across Slashing, Shock and
        // Magicka on the same hit, and across Slashing and Poison on another.
        let staggered = if self.has_staggered_weakness(now) {
            self.weakness_rating
        } else {
            0.0
        };
        gear + staggered
    }

    /// Combined flat resistance including transient Resist-Elements buffs (timed via
    /// `now`). The damage pipeline calls this instead of `resistance_against` so that
    /// the Resist-Elements flat reduction is applied AFTER block in the same step.
    pub fn total_resistance_against(&self, ty: DamageType, attacker_elem_pierce: f32, now: Instant) -> f32 {
        let perm = self.resistance_against(ty, attacker_elem_pierce);
        let transient = self.transient_resistance_against(ty, now);
        perm + transient
    }

    /// Max health WITHOUT the arena PvP health cheat — i.e. the character's own
    /// shipped pool. `max_health` is `(health_for_level(level) + equipped Fortify
    /// Health) × ARENA_HEALTH_MULTIPLIER`,
    /// and that multiplier is `PvpDefaultSettings.CHEAT_BASE_HEALTH_MULTIPLIER`: a
    /// pacing knob bolted onto the bar, not a change to the character's stats.
    pub fn base_max_health(&self) -> u32 {
        self.max_health / ARENA_HEALTH_MULTIPLIER.max(1)
    }

    /// The per-condition land threshold (absolute HP) for `condition`: the base
    /// `HEALTH_PERCENT_TO_CAUSE_STATUS × base_max_health`, RAISED by any matching
    /// `status_resist` ("Fortify Poisoned/…") bump. [§5.2 + §5.5]
    ///
    /// **Tracker #31.** This used `max_health`, i.e. the arena-TRIPLED bar, which
    /// made every elemental condition need 3× the shipped damage to land. The repo
    /// already flagged that as a known distortion
    /// (`docs/arena-status-resistance-spec.md` §5.2 "Arena ×3 caveat";
    /// `docs/arena-combat-fidelity-iteration.md`: "re-triggering from DoT alone is
    /// impossible in a single 5 s window"), and the retail capture settles the
    /// direction: in s615 (2026-06-27), `Frozen` (status 5) lands 31 times in one
    /// session on fighters who absorb single 400–780 damage hits, and it lands
    /// within ~1 s of EVERY Frostbite cast. Under `0.25 × tripled maxHP` the
    /// reporter's own match needed 810 accumulated frost against a 3240 HP
    /// opponent, and a full-rank-4 Frostbite channel delivers 287.4 — it could
    /// never fire.
    ///
    /// The `CHEAT_BASE_HEALTH_MULTIPLIER` is not part of the character's stats, so
    /// `_healthPercentToCauseStatus` — authored against the character's own pool —
    /// is read against the un-cheated pool.
    ///
    /// **This is a floor, not a pin.** The observed retail accumulations at the
    /// moment `Frozen` lands sit in the ~50–200 band (s470/s615, measured with a
    /// coarse accumulator that over-counts), i.e. still BELOW `0.25 × base maxHP`
    /// (270 at L89). Dropping the cheat multiplier moves us into the right order of
    /// magnitude without inventing a number; pinning the exact rule needs a
    /// dedicated pass over the capture corpus.
    pub fn condition_threshold(&self, condition: StatusEffectType) -> f32 {
        // Both terms are fractions of the SAME pool — the Fortify bump used to be a
        // fraction of the tripled bar while the base was too, so they matched; now
        // that the base is the un-cheated pool the bump follows it.
        let pool = self.base_max_health() as f32;
        let base = HEALTH_PERCENT_TO_CAUSE_STATUS * pool;
        let bump: f32 = self
            .loadout
            .status_resist
            .iter()
            .filter(|(c, _)| *c == condition)
            .map(|(_, frac)| *frac * pool)
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
            return NegationResult {
                negated: false,
                heal: 0.0,
                restore_magicka: 0.0,
                restore_cooldown_secs: 0.0,
            };
        }
        let health_before: f32 = components
            .iter()
            .filter(|(t, _)| super::damage::is_health_type(*t))
            .map(|(_, v)| *v)
            .sum();
        if health_before <= 0.0 {
            return NegationResult {
                negated: false,
                heal: 0.0,
                restore_magicka: 0.0,
                restore_cooldown_secs: 0.0,
            };
        }
        let mut heal = 0.0;
        let mut restore_magicka = 0.0;
        let mut restore_cooldown_secs = 0.0;
        for pool in self.negation_pools.iter_mut() {
            if pool.remaining <= 0.0 {
                continue;
            }
            // Which components may this pool touch at all? Ward is elemental-only
            // (see `NegationPool::elemental_only`); everything else keeps the old
            // "any health component" reach.
            let bypass = pool.bypass_types;
            let eligible_ty = |t: DamageType| {
                super::damage::is_health_type(t)
                    && (!pool.elemental_only || super::damage::is_elemental(t))
                    // `_vulnerableDamageTypes`: a Blizzard Armor does not stop fire.
                    && !bypass.contains(&(t as i32))
            };
            // Did this hit exhaust the pool? Only then does the overflow clause fire.
            let had_budget = pool.remaining > 0.0;
            // Drain this pool across the eligible health components (in order).
            for (ty, v) in components.iter_mut() {
                if !eligible_ty(*ty) || *v <= 0.0 || pool.remaining <= 0.0 {
                    continue;
                }
                // Only `absorb_fraction` of this component is eligible (1.0 for
                // Ward/Absorb/Dodge), and never more than the pool has left.
                let eligible = *v * pool.absorb_fraction.clamp(0.0, 1.0);
                let eaten = eligible.min(pool.remaining);
                *v -= eaten;
                pool.remaining -= eaten;
                heal += eaten * pool.restoration_factor;
                if eaten > 0.0 {
                    // This pool connected: pay its one-off restoration and disarm it
                    // so a multi-component hit cannot pay it several times.
                    let (h, m, c) = std::mem::take(&mut pool.on_absorb_restore);
                    heal += h;
                    restore_magicka += m;
                    restore_cooldown_secs += c;
                }
            }
            // "…plus any excess damage from the attack that destroys it." The pool
            // ran out DURING this hit, so the rest of the hit it was allowed to see
            // is negated too. Deliberately NOT healed back: the restoration factor is
            // paid on what the pool actually absorbed, and Ward (the only user of
            // this flag) heals nothing anyway.
            if pool.consumes_overflow && had_budget && pool.remaining <= 0.0 {
                for (ty, v) in components.iter_mut() {
                    if eligible_ty(*ty) && *v > 0.0 {
                        *v = 0.0;
                    }
                }
            }
        }
        self.negation_pools.retain(|p| p.remaining > 0.0);
        let health_after: f32 = components
            .iter()
            .filter(|(t, _)| super::damage::is_health_type(*t))
            .map(|(_, v)| *v)
            .sum();
        NegationResult {
            negated: health_after <= 0.0,
            heal,
            restore_magicka,
            restore_cooldown_secs,
        }
    }

    /// Drop negation pools whose duration has lapsed (call on tick / before a hit).
    pub fn prune_negation_pools(&mut self, now: Instant) {
        self.negation_pools.retain(|p| now < p.expires_at);
    }

    /// True iff this fighter is currently paralysed (its inputs are blocked).
    /// Is this fighter suffering one of the four ELEMENTAL conditions?
    ///
    /// The gate for `Opportunist` (PDOC / EDOC). Not a guess: the shipped asset
    /// carries `_triggerStatusEffects = [4, 5, 6, 7]` = Burning / Frozen / Enervated /
    /// Poisoned. Staggered(3), Blind(8) and Paralyzed(9) are modelled elsewhere and
    /// deliberately do NOT count.
    pub fn is_conditioned(&self, now: Instant) -> bool {
        const TRIGGERS: [StatusEffectType; 4] = [
            StatusEffectType::Burning,
            StatusEffectType::Frozen,
            StatusEffectType::Enervated,
            StatusEffectType::Poisoned,
        ];
        self.effects
            .iter()
            .any(|e| TRIGGERS.contains(&e.effect) && now < e.expires_at)
    }

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
    /// Magicka restored by a dodge that actually absorbed something
    /// (Renewing Dodge's `_maximumMagickaRestored`).
    pub restore_magicka: f32,
    /// Seconds to take off the dodger's own cooldowns
    /// (Focusing Dodge's `_maximumCooldownReduction`).
    pub restore_cooldown_secs: f32,
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
    /// `CurrentMatchStateTimeout` (propId6) paired with `match_state`. The same
    /// MatchState can carry different authored timeouts in round 0 and between
    /// rounds, so transport recovery must retain the exact value, not infer it.
    pub match_state_timeout_secs: f32,
    /// The match's `gameSessionId` (Match net-object propId9). Set by `MatchInstance`
    /// from the registry; a nil UUID until then (the binding gate is propId5, not 9).
    pub game_session_id: String,
    /// Next Avatar net object id to hand out (captures used 564–566).
    /// Channelled spells still delivering ticks — see [`ActiveChannel`].
    pub channels: Vec<ActiveChannel>,
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
    ///
    /// `None` is a REPLAYED double KO: a round that happened but that nobody won. The
    /// client models exactly that: `RoundInfo$$IsTied@0x2071a74` and
    /// `MatchPostRoundInfoMessage$$IsTied@0x1cebbd0` are "winner id and loser id both
    /// empty", and `MatchEndMatchMessage$$GetNumberOfRoundsWonBy@0x1cea200` counts
    /// round entries whose winner id equals the player. So a tied round is sent with
    /// empty ids and left out of the per-round arrays ([`Self::decided_round_results`]),
    /// which keeps the client's tally equal to `rounds_won`.
    pub round_winners: Vec<Option<usize>>,
    /// Double-KO rounds replayed so far this match (capped by [`MAX_DOUBLE_KO_REPLAYS`]).
    pub double_ko_replays: u8,
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
    /// Swings committed but not yet landed — see [`PendingHit`].
    pub pending_hits: Vec<PendingHit>,
    /// Echo Weapon follow-ups awaiting their `_weaponDelay`.
    pub pending_echoes: Vec<PendingEcho>,
    /// Casts waiting on their shipped wind-up. See [`PendingImpact`].
    pub pending_impacts: Vec<PendingImpact>,
}

/// A committed swing whose damage has not been applied yet.
///
/// WHY (tracker #21): the animation already walks AutoAttack → FollowThrough →
/// Recovery on retail's measured delays, but the DAMAGE was applied inline at
/// commit. So the wire said the hit lands 50 ms after the swing and the server
/// applied it immediately — and the defender's only reaction window was network
/// latency, which is what "the attack lands too early to make a high block" is.
///
/// Everything needed to resolve the hit is captured at commit, EXCEPT the
/// defender's guard. That is read when the hit lands, which is the entire point:
/// a block raised during the swing now counts.
/// A cast whose wind-up has not finished yet.
///
/// A spell that ships a `channelDuration` lands after it, not at the moment the
/// button is pressed. Ice Spike ships 1.12 s and Paralyze 1.5 s; applying their
/// damage and stun at cast time is what made a spike stun before it visibly left
/// the caster's hand.
#[derive(Debug, Clone)]
pub struct PendingImpact {
    pub sender: usize,
    pub target: usize,
    pub ability_uuid: String,
    pub level: u8,
    pub tag: AbilityTag,
    /// Maximum Power's condition, frozen when the cast was accepted (before its own
    /// magicka cost was deducted) — the order the client uses.
    pub magicka_full_at_cast: bool,
    pub due: Instant,
    /// When the cast was accepted; matches [`Execution::started_at`], so an
    /// interrupt of that execution drops this impact.
    pub cast_at: Instant,
    /// Maneuvers increment the combo when a weapon impact lands and reset it only
    /// when the maneuver is done. Multi-hit maneuvers therefore keep the chain
    /// alive until their final authored impact.
    pub reset_maneuver_combo_after: bool,
}

/// An **Echo Weapon** echo waiting to land: a flat follow-up hit `_weaponDelay`
/// after the swing that produced it.
#[derive(Debug, Clone)]
pub struct PendingEcho {
    pub sender: usize,
    pub target: usize,
    pub damage: f32,
    pub damage_type: DamageType,
    pub active_side: ActiveSide,
    pub due: Instant,
    pub expires_at: Instant,
}

#[derive(Debug, Clone)]
pub struct PendingHit {
    pub sender: usize,
    pub target: usize,
    pub side: ActiveSide,
    /// `1 + swing`, where `swing` is the charge plateau's factor (0 off it).
    pub swing_factor: f32,
    // No combo count: the hit reads the attacker's count when it LANDS, and the
    // count advances only then (combat-spec 02 X8).
    pub due: Instant,
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
            match_state_timeout_secs: 0.0,
            game_session_id: String::new(), // set by MatchInstance::new from the registry
            channels: Vec::new(),
            next_net_object_id: 564, // matches captured combat-actor id range
            round: 0,
            rounds_won: [0; 2],
            round_winners: Vec::new(),
            double_ko_replays: 0,
            phase_entered: now,
            winner: None,
            matchend_step: 0,
            interround_step: 0,
            last_regen_tick: now,
            pending_hits: Vec::new(),
            pending_echoes: Vec::new(),
            pending_impacts: Vec::new(),
        }
    }

    /// True iff some fighter has reached the best-of-3 round-win target (2). When this
    /// holds at a round-ending death, that death ends the MATCH; otherwise the match
    /// loops to the next round. `MaxMatchRounds` is 3 (`messages::MATCH_MAX_ROUNDS`,
    /// s506 Match propId8) → first to `ROUND_WINS_TO_WIN_MATCH` wins.
    pub fn match_is_won(&self) -> bool {
        self.rounds_won.iter().any(|&w| w >= ROUND_WINS_TO_WIN_MATCH)
    }

    /// The `(winner uuid, loser uuid)` of every DECIDED round, in order: the per-round
    /// array op48 and op49 carry. Replayed double KOs are left out (see
    /// [`Self::round_winners`]), so the array has at most best-of-3 entries.
    pub fn decided_round_results(&self) -> Vec<(String, String)> {
        let uuid = |s: usize| {
            self.fighters.get(s).map(|f| f.loadout.character_uuid.clone()).unwrap_or_default()
        };
        self.round_winners
            .iter()
            .flatten()
            .map(|&w| (uuid(w), uuid(1 - w)))
            .collect()
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

    /// The round winner when the authoritative InRound clock expires with both
    /// fighters alive — **AUTHORED, not capture-derived.** No recorded timeout round
    /// was available to pin the retail rule, so the tiebreak below is a designed
    /// fairness rule:
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

    /// Charge every equipped ability's **initial cooldown** for the round that is
    /// about to go live.
    ///
    /// `ActiveAbility._initialCooldown` ("cooldown charged at the start of a fight")
    /// is a first-use delay, distinct from the between-cast `_cooldown`, and
    /// `docs/arena-cooldowns-authoritative.md` records it as charged on ROUND start.
    /// The field was extracted into `gamedata.rs` for all 43 abilities that ship one
    /// and then never read by anything, so the gate simply did not exist: with the
    /// cooldown map empty at round start, an ability was castable on the round's
    /// first tick.
    ///
    /// Report: the AI opened round 2 with **Reckless Fury 1.35 s in** (match
    /// `7c8f70ac`, round live 19:36:03.271, cast 19:36:04.626). Reckless Fury has the
    /// longest initial cooldown in the game at 10.5 s — it should not have been
    /// available until 10.5 s. Stamina could not gate it either: the same reset
    /// refills pools to full, so its 425 cost was affordable immediately.
    ///
    /// Charged per round, not per match, because the pools, effects and cooldowns all
    /// reset per round — a round is the "fight" this field is charged at the start of.
    pub fn charge_initial_cooldowns(&mut self, now: Instant) {
        for f in &mut self.fighters {
            for a in &f.loadout.abilities {
                let Some(secs) =
                    super::tables::ability_initial_cooldown_secs(&a.instance_uuid, a.level)
                else {
                    continue;
                };
                if secs <= 0.0 {
                    continue;
                }
                // Never SHORTEN an existing entry: the between-round reset clears the
                // map first, so in practice this only ever inserts, but a future caller
                // must not be able to hand a fighter a discount.
                let until = now + std::time::Duration::from_secs_f32(secs);
                let e = f.cooldowns.entry(a.instance_uuid.clone()).or_insert(until);
                if *e < until {
                    *e = until;
                }
            }
        }
    }

    /// Reset both fighters to full pools for the next round (best-of-3 loop): HP/
    /// Stamina/Magicka back to max, clear cooldowns / status effects / block /
    /// swing-throttle, actor back to Idle. The stats sequence id keeps rising
    /// (monotonic across the whole match, as the wire expects). `round` is NOT
    /// touched here — the engine bumps it when the next round goes live.
    /// Return every fighter's ANIMATION to Idle, dropping any in-flight or scheduled
    /// transition, and leave the change queued for the caller to drain.
    ///
    /// Split out of [`Self::reset_fighters_for_next_round`] because the animation and
    /// the pools want different moments. The pools refill when the next round goes
    /// live; the animation has to stop when the round ENDS, or a fighter caught
    /// mid-cast holds that pose across the whole inter-round walk — report #113,
    /// where the opponent's cast pose survived "right through the break and up until
    /// the first strikes of the next round began", which is precisely where the
    /// combined reset used to fire.
    pub fn reset_actor_animations(&mut self, now: Instant) {
        self.reset_actor_animations_except(now, None);
    }

    /// As [`Self::reset_actor_animations`], leaving slot `keep` untouched.
    ///
    /// A non-final round's loser is that slot. The client shows a death only from
    /// op29, and `PvpAvatar$$CheckShouldForceServerState@0x1792864` skips op29 once a
    /// 39 carrying the same `…, Dead` indices has already been merged — so a 39 Idle
    /// drained ahead of op29 hid the death in rounds 1 and 2 (combat-spec 12 §7.2,
    /// 12-D5). No client code revives the dead actor between rounds either
    /// (`PvpAvatar$$EndRound@0x17848c0`); the loser stays Dead until
    /// [`Self::reset_fighters_for_next_round`] forces Idle at InRound.
    pub fn reset_actor_animations_except(&mut self, now: Instant, keep: Option<usize>) {
        for (slot, f) in self.fighters.iter_mut().enumerate() {
            if keep == Some(slot) {
                continue;
            }
            f.scheduled_states.clear();
            f.pending_manual_attack = None;
            f.active_manual_attack = None;
            f.channel_pose = None;
            if f.actor_state != ActorStateType::Idle {
                f.pending_state_changes.clear();
                f.set_actor_state(ActorStateType::Idle, now);
            }
            // An actor already logically Idle keeps its queue. At a round end that
            // queue holds the Idle `on_round_ended` forced to end an op53 cast pose;
            // clearing it here discarded that Idle unsent in every non-final round.
        }
    }

    pub fn reset_fighters_for_next_round(&mut self, now: Instant) {
        for f in &mut self.fighters {
            // Ravage does not cross a round boundary: give the ceiling back BEFORE
            // refilling, or the fighter would refill to the ravaged maximum and carry
            // the loss into a round it was never applied in.
            f.max_stamina += f.ravaged_stamina;
            f.max_magicka += f.ravaged_magicka;
            f.max_health += f.ravaged_health;
            f.ravaged_stamina = 0;
            f.ravaged_magicka = 0;
            f.ravaged_health = 0;
            f.health = f.max_health;
            f.stamina = f.max_stamina;
            f.magicka = f.max_magicka;
            f.stats_seq = f.stats_seq.wrapping_add(1);
            f.cooldowns.clear();
            f.effects.clear();
            // Drop every old transition, then explicitly tell both clients when an
            // in-flight animation returns to Idle. The client does not always rebuild
            // the actor between rounds: report #113 observed the opponent's final cast
            // pose survive the whole inter-round walk and into round 2. Resetting this
            // field silently left that client-side state stuck forever.
            f.pending_state_changes.clear();
            f.scheduled_states.clear();
            // The history ring and its index are per-ROUND (retail's firstIndex
            // restarts at 0 each round), so they reset with everything else.
            f.state_history.clear();
            f.transitions_total = 0;
            // The pose key is a transition count, and the count restarts here.
            f.channel_pose = None;
            f.force_actor_state(ActorStateType::Idle, now);
            f.blocking_side = ActiveSide::None;
            f.blocking_until = None;
            f.block_raised_at = None;
            f.last_block_dropped_at = None;
            f.last_swing = None;
            f.charge_press_at = None;
            f.charge_side = None;
            f.bot_swing_at = None;
            f.reset_combo();
            f.last_swing_side = ActiveSide::None;
            f.charge_began_at = None;
            f.recovery_began_at = None;
            // Phase 4.1: drop last round's pointer geometry so the first swing of the
            // new round can never be classified from a stale pre-reset sample.
            f.last_input_x = None;
            f.last_input_y = None;
            f.last_input_at = None;
            f.pending_manual_attack = None;
            f.active_manual_attack = None;
            f.last_client_charge = None;
            f.last_input_block_zone = None;
            f.damage_history.clear(); // ClearDamageHistory on round reset (§5.5)
            f.negation_pools.clear();
            f.transient_resistances.clear();
            f.transient_all_resistance.clear();
            f.maneuver_state_until = None;
            f.staggered_until = None;
            f.interrupt_pending = None;
            f.executions.clear();
            // A new round starts with nothing announced: the client resets its own
            // effect layer, so replaying removes for last round's statuses would be
            // noise at best and could clear a fresh apply at worst.
            f.announced_statuses.clear();
            f.status_timers.clear();
            f.announced_blocking = false;
            f.consumables_used = 0; // consumablesPerRound is PER ROUND [Phase 4.3]
            f.continuous_next_tick_at = None;
            f.continuous_carry = 0.0;
        }
        // These schedules belong to the MATCH rather than either Fighter, but their
        // contents are still per-round. Leaving them alive lets an old Frostbite
        // channel, a committed swing, or a spell impact land against the freshly
        // rebuilt avatars in the next round.
        self.channels.clear();
        self.pending_hits.clear();
        self.pending_impacts.clear();
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
    use std::time::Duration;

    // -----------------------------------------------------------------
    // op51 removes: the client is not told an effect ENDED
    // -----------------------------------------------------------------

    /// A stagger that lapses must show up as lapsed exactly once.
    ///
    /// THE REPORTED BUG: a player high-blocked a bot, the bot was stunned, and
    /// then swung at him and landed hits while still rendered mid-stun. The
    /// actor state went back to Idle on the wire; the status layer never heard.
    #[test]
    fn a_lapsed_stagger_is_reported_once_and_only_once() {
        let t0 = Instant::now();
        let mut f = Fighter::new(0, 564, Loadout::default(), t0);

        f.apply_stagger_for(t0, 1.0);
        // While it is running there is nothing to remove, and the status is
        // recorded as announced.
        assert!(f.drain_lapsed_statuses(t0).is_empty());
        assert!(f.drain_lapsed_statuses(t0 + Duration::from_millis(500)).is_empty());

        let lapsed = f.drain_lapsed_statuses(t0 + Duration::from_millis(1100));
        assert_eq!(lapsed, vec![StatusEffectType::Staggered], "the stagger must be reported");

        // Not again — a repeated remove would clear a fresh stagger applied later.
        assert!(
            f.drain_lapsed_statuses(t0 + Duration::from_millis(1200)).is_empty(),
            "a lapsed status must be reported once",
        );
    }

    /// Re-staggering before the first one lapses must not report a removal — the
    /// effect never stopped being active.
    #[test]
    fn a_refreshed_stagger_reports_nothing() {
        let t0 = Instant::now();
        let mut f = Fighter::new(0, 564, Loadout::default(), t0);
        f.apply_stagger_for(t0, 1.0);
        assert!(f.drain_lapsed_statuses(t0).is_empty());

        f.apply_stagger_for(t0 + Duration::from_millis(800), 1.0);
        assert!(
            f.drain_lapsed_statuses(t0 + Duration::from_millis(1100)).is_empty(),
            "the refresh extended it past 1.1s, so nothing lapsed",
        );
        assert_eq!(
            f.drain_lapsed_statuses(t0 + Duration::from_millis(1900)),
            vec![StatusEffectType::Staggered],
        );
    }

    /// Statuses the engine does not model the lifetime of must never be reported
    /// as lapsed — otherwise the first tick after a Ward would clear it.
    ///
    /// Ward / Absorb / ResistElements are announced from elsewhere and have no
    /// representation in what `tracked_statuses` scans.
    #[test]
    fn untracked_statuses_are_never_reported_as_lapsed() {
        let t0 = Instant::now();
        let mut f = Fighter::new(0, 564, Loadout::default(), t0);
        for _ in 0..3 {
            assert!(
                f.drain_lapsed_statuses(t0).is_empty(),
                "a fighter under no tracked status has nothing to remove",
            );
        }
        assert!(f.tracked_statuses(t0).is_empty());
    }

    use super::*;

    /// **The layout is pinned to real captured words, because nothing else was.**
    ///
    /// Health and Magicka sat swapped in `PackedStats` for the whole life of the
    /// arena server and the full 319-test suite stayed green, because every test
    /// either round-tripped `pack`/`unpack` against itself or passed a pre-packed
    /// literal straight through. A symmetric bug in a symmetric pair is invisible to
    /// a symmetric test. So this one asserts against bytes retail actually sent.
    ///
    /// Prod session 503, avatar 91, one round of `ReceiveDamage` (50) propId 4,
    /// frames 3503930 → 3504497 in order.
    ///
    /// Discriminators, strongest first: health is the pool that reads exactly 0 on the
    /// killing blow while the other two do not; a cast spends magicka and barely
    /// touches health; and health nets out at a collapse across the round while the
    /// regenerating pools recover.
    ///
    /// **Monotonicity is asserted only as a property of THIS fixture** (check 4). A
    /// regen perk plus rings/armour does let health rise mid-round — rare, and usually
    /// too slow to matter, which is why this fighter's trace never rises. The frames
    /// are hardcoded so the assertion is stable; do not lift it out as a general rule.
    #[test]
    fn packed_stats_layout_matches_retail_capture() {
        // (frame id, captured propId-4 ULong)
        const S503_AVATAR_91_ROUND: &[(u32, u64)] = &[
            (3_503_930, 0x3de2_0fff_0000_0030),
            (3_503_966, 0x3cc3_4613_0000_0044),
            (3_504_012, 0x37d0_02cd_0000_005e),
            (3_504_025, 0x3430_0292_0000_006c),
            (3_504_036, 0x2ec0_023b_0000_0076),
            (3_504_049, 0x2830_01b4_0000_0080),
            (3_504_062, 0x22b0_012d_0000_008a),
            (3_504_174, 0x1e80_857d_0000_00bc),
            (3_504_187, 0x18c0_d560_0000_00c6),
            (3_504_202, 0x1311_2142_0000_00d0),
            (3_504_212, 0x0d61_6d24_0000_00da),
            (3_504_365, 0x0780_4984_0000_011a),
            (3_504_398, 0x0541_1060_0000_012b),
            (3_504_482, 0x01a0_10db_0000_014e),
            (3_504_497, 0x0000_58f0_0000_015a), // the killing blow
        ];

        let decoded: Vec<(u32, (u16, u16, u16, u32))> = S503_AVATAR_91_ROUND
            .iter()
            .map(|(fid, v)| (*fid, PackedStats::unpack(*v)))
            .collect();

        // 1. THE decisive check: on the killing blow health is EXACTLY zero and the
        //    other two pools are not. Only one pool empties at death, and unlike
        //    monotonicity (check 4) that holds for every build.
        assert_eq!(decoded.first().unwrap().1 .0, 990, "health at the top of the round");
        let (fid, (h, s, m, _)) = *decoded.last().unwrap();
        assert_eq!(h, 0, "frame {fid} is the killing blow — health is 0");
        assert_eq!((s, m), (22, 240), "and the other two pools are NOT 0 there");

        // 2. The regenerating pools recover mid-round, while health nets out at a
        //    collapse. A comparison of net behaviour — not "health cannot rise".
        assert!(
            decoded.windows(2).any(|w| w[1].1 .2 > w[0].1 .2),
            "magicka must recover somewhere in the round — it regenerates at ~5%/s"
        );
        assert_eq!(
            (decoded.first().unwrap().1 .0, decoded.last().unwrap().1 .0),
            (990, 0),
            "health nets out at a collapse across the round"
        );

        // 3. A cast spends magicka. Frame 3503950 is a PlayerChannelingStateChange
        //    (53); the next stat word drops magicka 1023 → 531 while health only goes
        //    990 → 972. Reading these two fields the other way round would say the
        //    cast cost 18 health and healed 492 magicka.
        let before = PackedStats::unpack(0x3de2_0fff_0000_0030);
        let after = PackedStats::unpack(0x3cc3_4613_0000_0044);
        assert_eq!((before.2, after.2), (1023, 531), "the cast spent magicka");
        assert_eq!((before.0, after.0), (990, 972), "health barely moved across it");

        // 4. FIXTURE-SCOPED: this fighter had no health-regen source, so its health
        //    never rises. Kept as extra signal against a re-swap on these exact bytes —
        //    NOT because health cannot rise in a round. It can: a regen perk with the
        //    right rings/armour does it, just rarely and slowly. [owner, 2026-08-02]
        for w in decoded.windows(2) {
            let ((fid_a, a), (fid_b, b)) = (w[0], w[1]);
            assert!(
                b.0 <= a.0,
                "health rose in THIS fixture (frame {fid_a} = {}, frame {fid_b} = {}). \
                 These frames are hardcoded and this fighter had no regen source, so \
                 suspect the field order, not a regen build.",
                a.0,
                b.0,
            );
        }

        // 5. And the sequence id is still the LOW half — that part was always right.
        assert_eq!(decoded.first().unwrap().1 .3, 48);
        assert_eq!(decoded.last().unwrap().1 .3, 346);
    }

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
    /// Ravage takes the MAXIMUM down, not the current pool, and clamps current with it.
    #[test]
    fn ravage_lowers_the_ceiling_and_drags_a_full_pool_down_with_it() {
        let mut f = Fighter::new(0, 564, Loadout::default(), Instant::now());
        f.max_stamina = 660;
        f.stamina = 660;
        let (s, m, h) = f.apply_ravage(&[(DamageType::Stamina, 42.0)], 1.0);
        assert_eq!((s, m, h), (42, 0, 0));
        assert_eq!(f.max_stamina, 618, "the ceiling came down");
        assert_eq!(f.stamina, 618, "a full pool cannot sit above its own maximum");
        assert_eq!(f.ravaged_stamina, 42, "and the round remembers what to give back");
    }

    /// Per swing, and it accumulates — which is the whole point of the mechanic.
    ///
    /// Reckless Fury costs 425 stamina at rank 1. A versatile tier-10 Ravage Stamina
    /// takes 42 a swing, so six connected swings put a 660 ceiling under the cost and
    /// the ability becomes uncastable for the rest of the round.
    #[test]
    fn six_versatile_swings_put_reckless_fury_out_of_reach() {
        let mut f = Fighter::new(0, 564, Loadout::default(), Instant::now());
        f.max_stamina = 660;
        f.stamina = 660;
        const RECKLESS_FURY_COST: u32 = 425;
        assert!(f.max_stamina > RECKLESS_FURY_COST, "affordable before any ravage");
        for _ in 0..6 {
            f.apply_ravage(&[(DamageType::Stamina, 42.0)], 1.0);
        }
        assert_eq!(f.max_stamina, 660 - 6 * 42, "660 -> 408");
        assert!(
            f.max_stamina < RECKLESS_FURY_COST,
            "six swings must take Reckless Fury off the table: {} < {RECKLESS_FURY_COST}",
            f.max_stamina
        );
    }

    /// **Ravage Magicka denies Maximum Power.** This is the mechanic's whole point in
    /// high play: a Max-Power Ice Spike can stun through a Stahlrim shield, and
    /// ravaging the caster's magicka is how that is prevented.
    ///
    /// "Full" therefore means the pool's TRUE ceiling, not the ceiling ravage left
    /// behind — otherwise the victim would refill to the reduced maximum and keep the
    /// perk, which inverts the counter. `ravaged_magicka` is what remembers the
    /// difference. Not measurable from captures: ravage is absent from the wire.
    #[test]
    fn any_ravage_magicka_denies_maximum_power_for_the_round() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 564, Loadout::default(), now);
        f.max_magicka = 590;
        f.magicka = 590;
        assert!(
            crate::arena::combat::perks::CasterPerks::of(&f).magicka_full,
            "an untouched full pool is full"
        );

        f.apply_ravage(&[(DamageType::Magicka, 42.0)], 1.0);
        assert_eq!(f.max_magicka, 548);
        assert_eq!(f.magicka, 548, "current is clamped to the new ceiling");
        assert!(
            !crate::arena::combat::perks::CasterPerks::of(&f).magicka_full,
            "548/548 is NOT full while 42 of the ceiling is ravaged away — \
             Maximum Power is denied"
        );

        // And it comes back with the round, like every other ravage.
        let mut c = MatchCombat::new(2, 1, now);
        c.fighters.push(f);
        c.fighters.push(Fighter::new(1, 565, Loadout::default(), now));
        c.reset_fighters_for_next_round(now);
        assert!(
            crate::arena::combat::perks::CasterPerks::of(&c.fighters[0]).magicka_full,
            "round 2 starts on the true ceiling, so the perk is live again"
        );
    }

    /// Ravage Health takes the ceiling down and drags current health with it, but
    /// never to zero: ravage empties a pool, it does not execute.
    #[test]
    fn ravage_health_lowers_the_ceiling_without_killing() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 564, Loadout::default(), now);
        f.max_health = 300;
        f.health = 300;
        let (_, _, h) = f.apply_ravage(&[(DamageType::Health, 41.5)], 1.0);
        assert_eq!(h, 42, "41.5 rounds to 42");
        assert_eq!(f.max_health, 258);
        assert_eq!(f.health, 258, "a full bar comes down with the ceiling");

        // Enough swings to exhaust it must still leave the fighter alive.
        for _ in 0..20 {
            f.apply_ravage(&[(DamageType::Health, 41.5)], 1.0);
        }
        assert_eq!(f.max_health, 1, "floored at 1, never 0");
        assert!(!f.is_dead(), "ravage does not kill on its own");
    }

    /// Block scales it, because ravage rides the swing: an optimal block (physical
    /// factor 0) negates it outright, a late block takes a proportional bite.
    #[test]
    fn block_scales_ravage_and_an_optimal_block_negates_it() {
        let mut opt = Fighter::new(0, 564, Loadout::default(), Instant::now());
        opt.max_stamina = 660;
        opt.stamina = 660;
        assert_eq!(opt.apply_ravage(&[(DamageType::Stamina, 42.0)], 0.0), (0, 0, 0));
        assert_eq!(opt.max_stamina, 660, "an optimal block loses no ceiling");

        let mut late = Fighter::new(0, 564, Loadout::default(), Instant::now());
        late.max_stamina = 660;
        late.stamina = 660;
        let (s, _, _) = late.apply_ravage(&[(DamageType::Stamina, 42.0)], 0.5);
        assert_eq!(s, 21, "a half-reducing late block ravages half");
        assert_eq!(late.max_stamina, 639);
    }

    /// It does not cross a round boundary — the owner's rule, and the reason the
    /// removal is tracked separately from `max_stamina`.
    #[test]
    fn the_round_reset_hands_the_whole_ceiling_back() {
        let now = Instant::now();
        let mut c = MatchCombat::new(2, 1, now);
        c.fighters.push(Fighter::new(0, 564, Loadout::default(), now));
        c.fighters.push(Fighter::new(1, 565, Loadout::default(), now));
        c.fighters[0].max_stamina = 660;
        c.fighters[0].stamina = 660;
        c.fighters[0].max_magicka = 590;
        for _ in 0..4 {
            c.fighters[0].apply_ravage(
                &[(DamageType::Stamina, 42.0), (DamageType::Magicka, 42.0)],
                1.0,
            );
        }
        assert_eq!(c.fighters[0].max_stamina, 492, "ravaged during the round");
        assert_eq!(c.fighters[0].max_magicka, 422);

        c.reset_fighters_for_next_round(now);

        assert_eq!(c.fighters[0].max_stamina, 660, "round 2 starts on the full ceiling");
        assert_eq!(c.fighters[0].max_magicka, 590);
        assert_eq!(c.fighters[0].stamina, 660, "and refills to it, not to the ravaged max");
        assert_eq!(c.fighters[0].ravaged_stamina, 0);
        assert_eq!(c.fighters[0].ravaged_magicka, 0);
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
        // Flappety's live level-86 loadout has two tier-10 Fortify Health enchants:
        // (690 level pool + 2 × 110.64) × 3 = 2733.84 → 2734.
        let fortified = Fighter::new(
            0,
            564,
            Loadout { level: 86, max_health_bonus: 221.28, ..Default::default() },
            now,
        );
        assert_eq!(fortified.max_health, 2734);
        assert_eq!(fortified.base_max_health(), 911);
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

    /// COMBO state (combat-spec 02 §4.1-§4.3): the count is the number of connected
    /// hits in the chain; a swing that alternates sides keeps it, a repeated side
    /// starts a fresh one (R2), and `reset_combo` zeroes it. The count a hit reads
    /// is the one BEFORE its own increment.
    #[test]
    fn combo_counts_connected_hits_and_resets_on_a_repeated_side() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 564, Loadout::default(), now);
        let swing = |f: &mut Fighter, side| {
            let read = f.begin_combo_swing(side);
            f.increment_combo(); // the hit connects
            read
        };
        assert_eq!(swing(&mut f, ActiveSide::Right), 0, "first hit of a chain reads 0");
        assert_eq!(swing(&mut f, ActiveSide::Left), 1, "alternating → reads 1");
        assert_eq!(swing(&mut f, ActiveSide::Right), 2, "alternating → reads 2");
        assert_eq!(swing(&mut f, ActiveSide::Right), 0, "repeat side → fresh chain");
        // A swing that never connects does not advance the count (X8).
        assert_eq!(f.begin_combo_swing(ActiveSide::Left), 1);
        assert_eq!(f.combo_count, 1, "no increment until the hit lands");
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

        // Base Poisoned threshold = 25% of the character's OWN max HP; Fortify-Poisoned
        // raises it. Tracker #31: the arena `CHEAT_BASE_HEALTH_MULTIPLIER` must NOT
        // inflate it — under the old reading every elemental condition needed 3× the
        // shipped damage and Frostbite could never freeze anyone.
        let base_hp = f.base_max_health() as f32;
        assert!((max - base_hp * ARENA_HEALTH_MULTIPLIER as f32).abs() < 1e-2, "the bar IS tripled");
        let base = f.condition_threshold(StatusEffectType::Poisoned);
        assert!(
            (base - HEALTH_PERCENT_TO_CAUSE_STATUS * base_hp).abs() < 1e-2,
            "base threshold = 25% of the UN-cheated max HP"
        );
        assert!(base < HEALTH_PERCENT_TO_CAUSE_STATUS * max, "the arena health cheat does not raise it");
        f.loadout.status_resist = vec![(StatusEffectType::Poisoned, 0.10)];
        let bumped = f.condition_threshold(StatusEffectType::Poisoned);
        assert!(bumped > base, "Fortify Poisoned raises the threshold");
        assert!(
            (bumped - (HEALTH_PERCENT_TO_CAUSE_STATUS + 0.10) * base_hp).abs() < 1e-2,
            "the Fortify bump is a fraction of the same un-cheated pool"
        );
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
        assert_eq!(f.weakness_rating_against(DamageType::Poison, Instant::now()), 50.0);
        assert_eq!(f.weakness_rating_against(DamageType::Slashing, Instant::now()), 0.0);
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
            (OPTIMAL_BLOCK_RECOVERY_SECS - 0.8).abs() < 1e-6,
            "PvP OPTIMAL_BLOCK_RECOVERY_TIME is 0.8 s, got {OPTIMAL_BLOCK_RECOVERY_SECS}"
        );
        // 0.5 s is inside the 0.8 s PvP window; 0.9 s would now be OUTSIDE it.
        let still_late = now + std::time::Duration::from_millis(500);
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

    /// Resist Elements adds a transient flat resistance for all four elemental types;
    /// it is included in `total_resistance_against` and expires at its rank's shipped
    /// duration.
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

#[cfg(test)]
mod stun_duration_tests {
    use super::*;
    use crate::arena::combat::loadout;

    /// An ability's own `_stunDuration` must size the stagger, not the global default.
    /// IceSpike ships 1.20 s; `baseStaggerDuration` is 1.5 s — so before this, a stun
    /// from IceSpike lasted 25% longer than the game data says.
    #[test]
    fn an_abilitys_own_stun_duration_sizes_the_stagger() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.apply_stagger_for(now, 1.20);
        assert!(f.is_staggered(now + std::time::Duration::from_millis(1100)));
        assert!(
            !f.is_staggered(now + std::time::Duration::from_millis(1300)),
            "a 1.20s stun must be over by 1.3s — it was using the 1.5s global default"
        );
    }

    /// The no-duration path is unchanged: `apply_stagger` still means the shipped
    /// `baseStaggerDuration`, so nothing that relied on it moves.
    #[test]
    fn the_default_stagger_is_unchanged() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.apply_stagger(now);
        let d = std::time::Duration::from_secs_f32(BASE_STAGGER_DURATION_SECS);
        assert!(f.is_staggered(now + d - std::time::Duration::from_millis(50)));
        assert!(!f.is_staggered(now + d + std::time::Duration::from_millis(50)));
    }

    /// A stun still does everything a stagger does — guard down, combo broken, queued
    /// swing beats dropped. Those are what make it a stun rather than a debuff.
    #[test]
    fn a_stun_drops_the_guard_and_breaks_the_combo() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.blocking_until = Some(now + std::time::Duration::from_secs(5));
        f.begin_combo_swing(ActiveSide::Right);
        f.increment_combo();
        assert_eq!(f.combo_count, 1, "precondition: a live chain");
        f.apply_stagger_for(now, 1.20);
        assert!(f.blocking_until.is_none(), "guard must drop");
        assert_eq!(f.combo_count, 0, "combo must break");
        assert_eq!(f.actor_state(), ActorStateType::Staggered);
    }
}

#[cfg(test)]
mod absorb_fraction_tests {
    use super::*;
    use crate::arena::combat::loadout;

    fn pool(remaining: f32, fraction: f32) -> NegationPool {
        NegationPool {
            source: DamageNegationSource::Ward,
            remaining,
            expires_at: Instant::now() + std::time::Duration::from_secs(60),
            restoration_factor: 0.0,
            absorb_fraction: fraction,
            elemental_only: false,
            consumes_overflow: false,
            on_absorb_restore: (0.0, 0.0, 0.0),
            bypass_types: &[],
        }
    }

    /// A storm-armor shield eats HALF of each hit — the shipped
    /// `_damageAbsorptionPercent` is 0.50 at every rank. Absorbing the whole hit made
    /// it twice as strong and drained it twice as fast.
    #[test]
    fn a_half_absorbing_pool_lets_half_the_hit_through() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.negation_pools.push(pool(116.0, 0.5));
        let mut c = vec![(DamageType::Slashing, 100.0)];
        f.apply_negation_pools(&mut c);
        assert_eq!(c[0].1, 50.0, "half the hit must land");
        assert_eq!(f.negation_pools[0].remaining, 66.0, "and only half drains the pool");
    }

    /// Ward / Absorb / Dodge are unchanged: fraction 1.0 swallows the hit whole, which
    /// is what they did before the field existed. This is the additivity proof.
    #[test]
    fn a_full_absorbing_pool_behaves_exactly_as_before() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.negation_pools.push(pool(120.0, 1.0));
        let mut c = vec![(DamageType::Slashing, 100.0)];
        let r = f.apply_negation_pools(&mut c);
        assert_eq!(c[0].1, 0.0, "the whole hit is eaten");
        assert!(r.negated, "and the hit reports as negated");
        assert_eq!(f.negation_pools[0].remaining, 20.0);
    }

    /// The pool still caps at what it has left, fraction or not.
    #[test]
    fn a_nearly_empty_half_pool_cannot_overdraw() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.negation_pools.push(pool(10.0, 0.5));
        let mut c = vec![(DamageType::Slashing, 100.0)];
        f.apply_negation_pools(&mut c);
        assert_eq!(c[0].1, 90.0, "only the 10 it had left is absorbed");
        assert!(f.negation_pools.is_empty(), "an exhausted pool is dropped");
    }

    /// A real Ward: elemental-scoped, and it eats the breaking hit's overflow.
    fn ward_pool(remaining: f32) -> NegationPool {
        NegationPool {
            source: DamageNegationSource::Ward,
            remaining,
            expires_at: Instant::now() + std::time::Duration::from_secs(60),
            restoration_factor: 0.0,
            absorb_fraction: 1.0,
            elemental_only: true,
            consumes_overflow: true,
            on_absorb_restore: (0.0, 0.0, 0.0),
            bypass_types: &[],
        }
    }

    /// THE PRODUCTION BUG. `Ability.Spell.Ward.Description` says Ward negates
    /// "elemental damage" — its physical protection is the separate Armor Rating
    /// term. The pool used to drain on ANY health component, so weapon swings spent
    /// the elemental budget. In match dafb1378 a 120.54 Ward pool had been chewed up
    /// by physical hits and then let 112.37 poison through — damage it had more than
    /// enough nominal budget to stop.
    ///
    /// Fails on the old code: Slashing drained the pool to 20.
    #[test]
    fn a_ward_is_not_drained_by_physical_damage() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.negation_pools.push(ward_pool(120.54));
        let mut c = vec![(DamageType::Slashing, 100.0)];
        f.apply_negation_pools(&mut c);
        assert_eq!(c[0].1, 100.0, "physical must pass through a Ward untouched");
        assert_eq!(
            f.negation_pools[0].remaining, 120.54,
            "and must not spend one point of the elemental budget"
        );
    }

    /// The other half of the same production event: having survived the swings, the
    /// Ward must then actually stop the poison it was cast for.
    #[test]
    fn a_ward_still_stops_the_poison_after_taking_physical_hits() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.negation_pools.push(ward_pool(120.54));
        let mut swing = vec![(DamageType::Slashing, 100.0)];
        f.apply_negation_pools(&mut swing);
        let mut spell = vec![(DamageType::Poison, 112.37)];
        let r = f.apply_negation_pools(&mut spell);
        assert_eq!(spell[0].1, 0.0, "the poison must be fully negated");
        assert!(r.negated);
    }

    /// "…plus any excess damage from the attack that destroys it." A 176.59 Ward hit
    /// for 300 Fire must negate all 300, not leak 123.41.
    ///
    /// Fails on the old code: 123.41 landed.
    #[test]
    fn a_ward_negates_the_overflow_of_the_hit_that_breaks_it() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.negation_pools.push(ward_pool(176.59));
        let mut c = vec![(DamageType::Fire, 300.0)];
        let r = f.apply_negation_pools(&mut c);
        assert_eq!(c[0].1, 0.0, "the breaking hit's excess is negated too");
        assert!(r.negated, "so the hit reports as fully negated");
        assert!(f.negation_pools.is_empty(), "and the spent Ward is gone");
    }

    /// The overflow clause is scoped to what the pool could touch. A mixed hit on an
    /// elemental-only Ward must still let the PHYSICAL half land — otherwise the
    /// overflow fix would hand Ward the physical immunity it never had.
    #[test]
    fn a_broken_ward_does_not_swallow_the_physical_half_of_a_mixed_hit() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.negation_pools.push(ward_pool(50.0));
        let mut c = vec![(DamageType::Fire, 300.0), (DamageType::Slashing, 80.0)];
        let r = f.apply_negation_pools(&mut c);
        assert_eq!(c[0].1, 0.0, "all Fire is negated (budget + overflow)");
        assert_eq!(c[1].1, 80.0, "but the physical component is untouched");
        assert!(!r.negated, "and the hit is NOT a full negation");
    }

    /// The overflow clause must not fire on a pool that was ALREADY empty before the
    /// hit — only on the hit that destroys it. (An already-drained pool is retained
    /// only until the retain() below; this guards the ordering.)
    #[test]
    fn an_already_spent_ward_does_not_negate_a_later_hit() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.negation_pools.push(ward_pool(50.0));
        let mut first = vec![(DamageType::Fire, 300.0)];
        f.apply_negation_pools(&mut first);
        assert!(f.negation_pools.is_empty(), "precondition: the Ward is spent");
        let mut second = vec![(DamageType::Fire, 200.0)];
        let r = f.apply_negation_pools(&mut second);
        assert_eq!(second[0].1, 200.0, "the next hit lands in full");
        assert!(!r.negated);
    }

    /// Adrenaline / Renewing / Focusing Dodge each ship a restoration the server read
    /// from nobody, so all three were identical to a plain Dodging Strike: they cost
    /// their stamina and gave only the dodge. The payout is conditional on the dodge
    /// CONNECTING, and is paid once per dodge, not once per damage component.
    #[test]
    fn a_connecting_dodge_pays_its_authored_restoration_exactly_once() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.negation_pools.push(NegationPool {
            source: DamageNegationSource::Dodge,
            remaining: 500.0,
            expires_at: now + std::time::Duration::from_secs(1),
            restoration_factor: 0.0,
            absorb_fraction: 1.0,
            elemental_only: false,
            consumes_overflow: false,
            on_absorb_restore: (43.5, 338.0, 4.0),
            bypass_types: &[],
        });
        // A multi-component hit: the restoration must NOT be paid per component.
        let mut c = vec![
            (DamageType::Slashing, 40.0),
            (DamageType::Fire, 30.0),
            (DamageType::Poison, 20.0),
        ];
        let r = f.apply_negation_pools(&mut c);
        assert!((r.heal - 43.5).abs() < 1e-3, "health paid once, got {}", r.heal);
        assert!((r.restore_magicka - 338.0).abs() < 1e-3, "magicka paid once");
        assert!((r.restore_cooldown_secs - 4.0).abs() < 1e-3, "cooldown paid once");
    }

    /// A dodge that never connects pays nothing — the reward is for a dodge that
    /// actually absorbed a hit, not for casting.
    #[test]
    fn a_dodge_that_absorbs_nothing_pays_nothing() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.negation_pools.push(NegationPool {
            source: DamageNegationSource::Dodge,
            remaining: 500.0,
            expires_at: now + std::time::Duration::from_secs(1),
            restoration_factor: 0.0,
            absorb_fraction: 1.0,
            elemental_only: false,
            consumes_overflow: false,
            on_absorb_restore: (43.5, 338.0, 4.0),
            bypass_types: &[],
        });
        // Only a Magicka drain — not a health-type component, so nothing is absorbed.
        let mut c = vec![(DamageType::Magicka, 100.0)];
        let r = f.apply_negation_pools(&mut c);
        assert_eq!(r.heal, 0.0);
        assert_eq!(r.restore_magicka, 0.0);
        assert_eq!(r.restore_cooldown_secs, 0.0);
    }

    /// **Powerful Block / StaggeredWeakness.** The attacker holds the status and it
    /// amplifies damage they TAKE, by a FLAT amount shared by every damage type of
    /// the hit — the client's `GetWeakness(DamageType)` is two instructions and never
    /// reads its type argument.
    #[test]
    fn staggered_weakness_is_flat_and_type_agnostic() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        // Stagger first, then the weakness — the order `stun_the_blocked_attacker`
        // uses; a fresh stagger starts with no weakness.
        assert!(f.apply_stagger_for(now, 2.5));
        f.weakness_rating = 50.4; // tier 10
        assert!(f.has_staggered_weakness(now));
        for ty in [DamageType::Slashing, DamageType::Poison, DamageType::Shock] {
            assert_eq!(
                f.weakness_rating_against(ty, now),
                50.4,
                "{ty:?} must see the same flat rating — the effect is type-agnostic"
            );
        }
    }

    /// It has NO duration of its own (every captured op51 carries 0.0) and lives
    /// exactly as long as the Staggered that produced it.
    #[test]
    fn staggered_weakness_dies_with_the_stagger() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        assert!(f.apply_stagger_for(now, 2.5));
        f.weakness_rating = 50.4;
        assert!(f.has_staggered_weakness(now));
        let after = now + std::time::Duration::from_secs_f32(2.6);
        assert!(!f.is_staggered(after), "precondition: the stagger has lapsed");
        assert!(
            !f.has_staggered_weakness(after),
            "the weakness must lapse with it, not outlive it"
        );
        assert_eq!(f.weakness_rating_against(DamageType::Slashing, after), 0.0);
    }

    /// A fighter with no Powerful Block on the other side is never weakened — the
    /// negative control that stops this firing on every stagger. Only 10 of 245
    /// block-recoil staggers in the retail corpus carry the status.
    #[test]
    fn an_ordinary_stagger_carries_no_weakness() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.apply_stagger_for(now, 2.5);
        assert!(f.is_staggered(now));
        assert!(!f.has_staggered_weakness(now), "a plain stagger does not amplify");
        assert_eq!(f.weakness_rating_against(DamageType::Slashing, now), 0.0);
    }

    /// Absorb is NOT element-scoped and has no overflow clause — the additivity proof
    /// that the two new flags changed nothing for the other pool kinds.
    #[test]
    fn an_absorb_pool_still_eats_physical_and_still_leaks_overflow() {
        let now = Instant::now();
        let mut f = Fighter::new(0, 1, loadout::starter(), now);
        f.negation_pools.push(NegationPool {
            source: DamageNegationSource::Absorb,
            remaining: 50.0,
            expires_at: now + std::time::Duration::from_secs(60),
            restoration_factor: 1.0,
            absorb_fraction: 1.0,
            elemental_only: false,
            consumes_overflow: false,
            on_absorb_restore: (0.0, 0.0, 0.0),
            bypass_types: &[],
        });
        let mut c = vec![(DamageType::Slashing, 130.0)];
        let r = f.apply_negation_pools(&mut c);
        assert_eq!(c[0].1, 80.0, "Absorb eats physical, and the excess still lands");
        assert!((r.heal - 50.0).abs() < 1e-3, "and heals back what it ate");
    }
}
