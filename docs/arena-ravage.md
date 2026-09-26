# Ravage — max-pool reduction

"Reduces target's maximum Stamina/Magicka by {0}." Six shipped property logics:
`Weapon`/`Shield` x `RavageStamina`/`RavageMagicka`/`RavageHealth`.

Distinct from the *drain* families (`WeaponDamageStaminaPropertyLogic`, "Does {0}
damage to Magicka"), which take the CURRENT pool and regenerate back. A ravage does
not come back inside the round.

## Magnitudes (both families share one curve)

Weight-scaled — the shipped family carries three tables:

| tier 10 | light | versatile | heavy |
| --- | --- | --- | --- |
| Weapon Ravage Stamina / Magicka | 31.66 | 42.00 | 52.66 |
| Weapon Ravage Health | 31.66 | **41.50** | 52.66 |

Ravage Health's versatile column is 41.50, not 42.00 — the families are not
interchangeable, so each is read from its own table.

The **shield** families carry no weapon weight and ship a single curve: 52.66 at
tier 10 for Stamina, Magicka and Health alike.

Read with `gamedata::enchant_magnitude_for_weight`. Reading the base table for a
versatile or heavy weapon under-reports by a quarter to a third.

**Thunderfell** (`90381284…`, "Shock Mace", `weaponClass` 2 = versatile) carries
Weapon Shock Damage + **both** ravage suffixes at tier 10 — 42 off each pool per swing.

## It is not on the wire

Measured over the whole corpus: 39,744 op50 `ReceiveDamage` frames, 101 sessions,
20,469 after dedup. **Nothing announces a ravage.** Controls:

- The encoding is capable — damage **type 8 (Stamina)** appears 4,081 times and type
  9 (Magicka) 1,479 times as `damageByType` components.
- Those components do not track ravage: sessions whose participants carry a Weapon
  Ravage enchant show a type-8 component 47/70 (67%); sessions carrying none show it
  23/34 (68%). 23 no-ravage sessions have type-8; 23 ravage sessions have none.
- s506's attacker demonstrably carries Weapon Ravage Stamina t10 and lands 370 hits —
  **zero** type-8 components.
- s615, the largest type-8 session (477), carries only Ravage **Magicka**.
- `STATUS_EFFECT` has no Ravage entry.

Those type-8/9 components are the mirrored Frost→Stamina / Shock→Magicka drains
(`append_mirrored_drains`), which is why they are damage-scaled across 616 distinct
values and independent of ravage.

**Why retail can stay silent.** op50 pools are 10-bit *fractions of max*
(`packed_stats`). Shrinking max while clamping current to it leaves the fraction
unchanged, so no bar can show it — and the arena UI draws no opponent stamina or
magicka bar at all. Players who use this track the numbers in their heads.

## The model

- The **weapon** families apply **per landed swing**, from the attacker's loadout onto
  the victim's maximum — weapon-based sources only (`Attack`, `WeaponManeuver`):
  `WeaponRavageBonusInstance$$Ravage@0x1D5495C` returns early unless
  `CombatManager.IsWeaponBased@0x1BD3E6C` (`(source | 2) == 3`). A spell or a channel
  tick never ravages; before report #231 every 0.2 s Frostbite / Blizzard Armor tick did.
- The **shield** families fire on the opposite event — *"on a blocked attack or Shield
  Bash"* — and ravage whoever swung into the guard. They are held in a separate
  `shield_ravage` list so the resolver cannot apply one as if it were the other.
- Current pool is clamped down with the ceiling.
- Scaled by the hit's **physical block factor**: a connected optimal block (physical
  x0) negates it, a late block takes a proportional bite. A dodged swing resolves no
  hit, so it never reaches the ravage path.
- **Does not cross a round.** `reset_fighters_for_next_round` hands the whole ceiling
  back before refilling, so round 2 starts clean.
- Emits no new wire message.

### What is authored rather than measured

The block scaling. Ravage is absent from the captures, so no capture can settle
whether retail reduces it on a block, negates it, or ignores the guard. Riding the
swing's own physical factor is the assumption; it matches the owner's report that a
high block modifies it and is the behaviour the tests pin. If a tester shows
otherwise, `apply_ravage`'s `factor` argument is the single place to change.

**Ravage Health** is applied on the same terms. The arena's x3 health cheat multiplies
the POOL, not the effects acting on it, so the cut is flat against a tripled ceiling.
The maximum is floored at 1: a zero maximum would make `wire_fraction` divide by zero
and read as dead with nobody landing a blow. Ravage empties a pool, it does not
execute.

## Maximum Power

Maximum Power requires a full magicka pool, and "full" is measured against the pool's
TRUE ceiling (`max_magicka + ravaged_magicka`) — **so any Ravage Magicka landing in a
round denies the perk for the rest of that round.**

Measuring against the ravaged ceiling instead would let the victim refill to the
reduced maximum and keep the perk, which inverts the counter: ravaging magicka is
precisely how Maximum Power is denied in high play, where a Max-Power Ice Spike can
stun through a Stahlrim shield.

This is **not measurable from captures** — ravage is absent from the wire, so no
session can show a ravaged caster's perk state. It follows the owner's reading, which
the perk's own shipped text already agreed with ("if ravaged, then MP is void").

## Why it matters

Reckless Fury costs 425 stamina at rank 1. Against a 660 ceiling, six connected
versatile tier-10 swings (or five heavy) put the maximum under the cost and take the
ability off the table for the rest of the round. Maximum Power requires a full magicka
pool, so a ravage landing on a partly-spent pool voids the perk until it refills to the
new ceiling. 30 characters in the capture corpus carry Ravage Stamina, 24 Ravage
Magicka.
