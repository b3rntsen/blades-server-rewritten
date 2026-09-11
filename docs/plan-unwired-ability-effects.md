# Plan: the ability effects the server still ignores

**Status: COMPLETE.** All twelve unread fields are resolved — eleven wired,
`projectile_speed` deliberately not (client-side presentation; see §7).

Two things this plan got wrong, both mine, both caught by looking at the data:
* `damage_reduction` DOES ship a duration (`_blockDuration` 0.50 s) — I had only
  surveyed a subset of the fields.
* §5 was described as the invasive, risky one. Half of it needed **no pipeline change
  at all**: `armor_piercing_rating` and `elem_resist_piercing_rating` were already
  consumed and simply never set. Only block piercing needed the threading.

Two values were recovered from `dump.cs:609793-609832` rather than guessed:
`Blind = 8` and `ElementalStormArmor = 16` (one shared value for all three `*Armor`
spells). Neither is capture-confirmed — nobody cast them in the sessions we hold — but
propId 5 matched that enum 2,965/2,965 times, so the reading is well-founded.

**Known remaining limits, none of them a dead field:**
* Block piercing weakens a LATE block only. A connected OPTIMAL block still negates
  physical outright, because that zero is capture-pinned; whether retail pierces an
  optimal block is unpinned.
* ~~Storm armor is a flat absorb pool.~~ **FIXED:** `_damageAbsorptionPercent` ships
  **0.50 at every rank**, so the shield now eats HALF of each hit until its 116-158
  pool drains. `_vulnerableDamageTypes` (the elemental weakness the shield opens) is
  still unwired — it needs a per-type vulnerability on the fighter, which nothing has.
* The weakness block (100-103) is transcribed but nothing is wired to it — the
  propId5-vs-propId12 ambiguity is unresolved.

**Why it matters:** the goal is to keep the game playable for players after shutdown.
An ability that spends a resource and does nothing is worse than an ability that
doesn't exist — the player thinks the server is broken, because it is.

## How this list was produced

Every accessor on a shipped ability rank was grepped against the whole engine to see
whether anything reads it. Twelve were read by nothing. Then each was surveyed across
the full `gamedata::ABILITIES` table to find which abilities actually ship a value —
because a field no ability uses is not worth wiring.

| field | abilities that ship it | status |
|---|---|---|
| `stun_duration` | StaggeringBash, Guardbreaker, IceSpike | **DONE** — sizes the stagger |
| `shield_health` | TempestArmor, FirestormArmor, BlizzardArmor | planned — §1 |
| `maximum_damage_dodged` | DodgingStrike, RenewingDodge, AdrenalineDodge, FocusingDodge | planned — §2 |
| `damage_reduction` | ShieldOfMania, ReflectingBash | planned — §3 |
| `freeze_duration` + `paralyze_duration` | FlashFreeze | planned — §4 |
| `armor_piercing_percent`, `block_piercing_percent` | Skullcrusher | planned — §5 |
| `elemental_block_piercing`, `elemental_resistance_piercing` | PiercingStrikes | planned — §5 |
| `damage_to_cause_blind` | Blind | **BLOCKED** — §6 |
| `projectile_speed` | Blind, Paralyze, IceSpike, Fireball | **won't do** — §7 |

Six abilities currently cost a resource and produce nothing at all: the three `*Armor`
spells, SnakeBite, MagickaSurge, EchoWeapon. Sections 1 and 3 cover half of them.

---

## §1 — `shield_health`: the three `*Armor` spells (highest value)

Firestorm / Blizzard / Tempest Armor are **damage-shield auras**: a pool that absorbs
incoming damage, plus a `damage_per_second` that burns whoever attacks the caster.
Today they are tagged `Damage`, routed to the direct-damage path, and resolve to zero —
so they are the clearest "spent 300 magicka, nothing happened" in the game.

Wire as a negation pool, which already exists and is tested. `Fighter::negation_pools`
+ `apply_negation_pools` are what Ward and Absorb use; an Armor spell is the same shape
with a different source:

1. `ability_tag_for_template` — route `*Armor` to a new `AbilityTag::ElementalArmor`
   (do NOT reuse `Ward`: Ward's pool is `ward_health`/`ward_armor`/`ward_duration`,
   a different field set).
2. `apply_elemental_armor(combat, caster, level, now)` alongside `apply_ward` — push a
   pool of `shield_health` for `duration`, and emit op51 with the matching status.
3. The retaliation dps is the second half and is **not** in scope for a first pass: it
   needs an attacker-side hook in `emit_damage` (when the defender has an armor pool,
   deal `dps × tick` back to the attacker). Ship the absorb first; it is the part the
   player feels.

Open question: which `StatusEffectType` the client expects. `Ward = 15` and
`Absorb = 17` are pinned; the elemental-armor value is not. Check dump.cs before
emitting op51, and if it can't be found, apply the pool server-side and skip the status
frame rather than guessing an id — a wrong id is silently dropped.

## §2 — `maximum_damage_dodged`: the four Dodge abilities

`StatusEffectType::Dodging = 12` is already pinned, and the negation-pool machinery
mentions Dodge. These four ship a cap on how much a dodge can absorb. Same shape as
§1: a pool of `maximum_damage_dodged`, expiring on `duration`, emitted as op51 with the
known status id. Lower risk than §1 because the status value is already known.

`RenewingDodge` / `AdrenalineDodge` / `FocusingDodge` likely carry a secondary effect
(heal / stamina / magicka on a successful dodge) — check which other fields they ship
before assuming the cap is the whole ability.

## §3 — `damage_reduction`: ShieldOfMania, ReflectingBash

A flat or fractional reduction on incoming damage for a duration. `transient_resistances`
on `Fighter` already exists for exactly this class of temporary defensive buff (that is
how Resist-Elements works) and expires on its own.

Unknown: whether the shipped number is a fraction (0.30 = 30 % less) or a flat rating
subtraction. Both readings are plausible from the field name. **Decide from the value
range** — survey the shipped numbers across ranks first: values ≤ 1.0 across all ranks
mean a fraction, values in the tens or hundreds mean a flat rating. Do not implement
before that check; getting it backwards makes a defensive buff either useless or
invincible.

ReflectingBash also implies reflecting damage back, which is the §1 retaliation hook —
build them together.

## §4 — `freeze_duration` + `paralyze_duration`: FlashFreeze

FlashFreeze is the only ability shipping both, and both are unread. `StatusEffectType`
already has `Frozen = 5` and `Paralyzed = 9`, and the engine already has a full
paralysis implementation (`ActorStateType::Paralyzed`, `paralyze_secs`, `try_paralyze`,
input locking).

The wiring is small: on a landed FlashFreeze, apply `Frozen` for `freeze_duration` and
`Paralyzed` for `paralyze_duration` — using the ability's own numbers instead of
`paralyze_duration_secs(rank)`, which is a separate table and probably not what
FlashFreeze intends. Follow the pattern the `stun_duration` change just set: prefer the
ability's shipped duration, fall back to the generic one.

## §5 — the piercing fields: Skullcrusher, PiercingStrikes

`armor_piercing_percent`, `block_piercing_percent`, `elemental_block_piercing`,
`elemental_resistance_piercing`. These modify an **attack**, not the attacker — they
reduce how much of the defender's armor / block / resistance applies to this hit.

More invasive than §1-§4: it means threading a piercing parameter through
`resolve_attack` → `finish_resolved` → the block and resistance stages. Those stages are
capture-calibrated (`roundtrip_s506_damage`), so the change must be additive — a
piercing of 0 must produce byte-identical results to today, and the existing
differential tests are the proof of that.

Do this LAST. It touches the one part of the damage model that is pinned against
recorded retail values, and §1-§4 deliver more player-visible value per unit of risk.

## §6 — `damage_to_cause_blind`: DONE

Blind ships a damage threshold exactly parallel to `damage_to_cause_paralyze`.
Capture and enum evidence now pin Blind to status id 8. A qualifying hit emits op51
on the victim's Avatar, and the server keeps an `ActiveEffect` for the shipped
duration so normal expiry and Indomitable Smash's `[4,5,6,7,8]` cure both emit the
matching op51 remove. Resist Elements has the narrower shipped `[4,5,6,7]` list and
does not cure Blind. Regression tests cover target, source kind, duration and both
viewers.

## §7 — `projectile_speed`: deliberately not wiring

Blind, Paralyze, IceSpike, Fireball ship a projectile speed. This is **client-side
presentation** — how fast the visual travels. The server resolves a cast as an
instantaneous authoritative event and retail's own captures show the damage message
arriving without any travel delay. Modelling flight time server-side would add latency
to every spell for no gameplay gain and would desync the damage from the animation.

Leaving it unread is correct. Recorded here so nobody re-discovers it as a gap.

---

## Suggested order

1. **§4 FlashFreeze** — smallest, both status ids known, follows the pattern just set.
2. **§2 Dodge** — status id known, pool machinery exists.
3. **§1 `*Armor` absorb** — highest player value; needs one dump.cs lookup first.
4. **§3 `damage_reduction`** — needs the fraction-vs-flat survey before any code.
5. **§1/§3 retaliation** — the reflect/burn-back hook, once §1 and §3 exist.
6. **§5 piercing** — last, additive, guarded by the s506 differentials.
7. **§6 Blind** — complete; status id and lifecycle are capture-pinned.

## The rule this list was written under

Every one of these has a shipped number, so none of them needs a number invented. Where
a value is genuinely missing — the blind status id, the elemental-armor status id, the
fraction-vs-flat reading of `damage_reduction` — the plan says *find it or stop*, not
*pick something plausible*. Three separate bugs this week came from a plausible guess
that nothing verified: the packed-stats field order, the maneuver damage path, and the
swing that never sent its wind-up.
