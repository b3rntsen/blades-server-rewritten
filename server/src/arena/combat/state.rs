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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    PlayerAutoAttack = 19,
    Emote = 28,
    // … (Recovery / FollowThrough / Charging / Draining / Maneuver discriminants
    //    TBD from dump.cs when those state-change messages are built).
}

/// `StatusEffectType` — combat status effects (`ChangeCombatStatusEffect`).
/// Partially mapped; the full enum is large.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum StatusEffectType {
    None = 0,
    Blocking = 1,
    Burning = 4,
    Poisoned = 7,
    BlockStaminaRegen = 51,
    // …
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

/// One equipped ability: its instance UUID (as referenced by
/// `RequestExecuteAbility`) and its level (drives scaling/cooldown).
#[derive(Debug, Clone)]
pub struct EquippedAbility {
    pub instance_uuid: String,
    pub level: u8,
}

/// The weapon's base damage profile (per-type), filled from game data / RE.
#[derive(Debug, Clone, Default)]
pub struct WeaponProfile {
    pub primary_type: Option<DamageType>,
    /// Base damage per type before swing/ability/enchant factors.
    pub base_by_type: Vec<(DamageType, f32)>,
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
    pub expires_at: Instant,
    pub last_tick: Instant,
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
    /// Time of this fighter's last landed swing (combat throttle / swing cadence).
    pub last_swing: Option<Instant>,
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
            actor_state: ActorStateType::Idle,
            state_entered: now,
            arena_target: 1 - slot.min(1), // 2-player: the other slot
            blocking_side: ActiveSide::None,
            last_swing: None,
        }
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
    /// Next Avatar net object id to hand out (captures used 564–566).
    pub next_net_object_id: i32,
    pub round: u8,
    pub rounds_won: [u8; 2],
    /// When the current flow phase started (drives StateTimeout heartbeat /
    /// round timers from the tick).
    pub phase_entered: Instant,
}

impl MatchCombat {
    pub fn new(capacity: usize, expected_peers: usize, now: Instant) -> Self {
        MatchCombat {
            phase: FlowState::Connecting,
            capacity,
            expected_peers,
            fighters: Vec::with_capacity(capacity),
            flow_controller_id: 560, // matches captured flow-controller id range
            next_net_object_id: 564, // matches captured combat-actor id range
            round: 0,
            rounds_won: [0; 2],
            phase_entered: now,
        }
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
}
