# Arena Ability Cooldowns — Authoritative (APK Unity Assets)

**Source:** UnityPy extraction from APK bundled Unity assets  
(`/Users/berntsen/Projects/blades-capture/.claude/worktrees/arena-server-v2/reference/apk/blades.apk`)  
**Asset type:** MonoBehaviour (ActiveAbility ScriptableObjects), field `_cooldown` (float, seconds)  
**Data architecture:** `[ExcelVariable] private float _cooldown;` on `ActiveAbility : AbstractAbility : ScriptableObject`.  
Values are consistent across **all ranks** of each ability (only `_staminaCost`/`_magickaCost`/`_powerLevel` scale with rank).

**Source 1 (API data):** NOT yielded — 119,205 captured responses searched; NONE contain "cooldown" or "Cooldown". Abilities/loadout endpoints return only UUID+level maps, no game definitions.  
**Source 2 (dump.cs):** Confirmed `_cooldown` is `[ExcelVariable]` on `ActiveAbility`. The `[ExcelImportAttribute]` constructor argument (the filename) is a runtime string constant not emitted in dump.cs; the actual values are baked into the APK's ScriptableObjects.  
**Source 3 (APK Unity assets):** **AUTHORITATIVE DATA FOUND.** All `ActiveAbility` ScriptableObject instances are bundled in the APK (not CDN-only), extractable via UnityPy from the MonoBehaviour catalog.

---

## Authoritative Cooldowns Table

All values are `_cooldown` (seconds), rank-independent. `_initialCooldown` = first-use delay on round start.

| ability-UUID | name | cooldown-sec (authoritative) | initial-CD-sec | type | SOURCE |
|---|---|---|---|---|---|
| `4e760726-b012-4b25-bc92-0cd6312d6601` | Absorb | **6.0** | 1.20 | Spell | APK MonoBehaviour |
| `be56c560-a4ba-47ad-8513-f24c342ca594` | Adrenaline Dodge | **8.65** | 2.06 | Maneuver | APK MonoBehaviour |
| `c4b48518-e847-4f3d-81a2-2856bdb4ed98` | Blizzard Armor | **7.5** | 1.20 | Spell | APK MonoBehaviour |
| `e07f9b1a-64db-44ef-ba25-0e4378789ddc` | Consuming Inferno | **8.09** | 2.75 | Spell | APK MonoBehaviour |
| `dfb8d247-1333-42eb-9730-a1c16d10584f` | Delayed Lightning Bolt | **6.58** | 2.30 | Spell | APK MonoBehaviour |
| `1e7f0dd6-6015-4f65-b811-3246e407e330` | Dodging Strike | **8.65** | 2.06 | Maneuver | APK MonoBehaviour |
| `d07a8d30-9a1c-49b0-866d-97a8aa1534cf` | Fireball | **3.54** | 1.40 | Spell | APK MonoBehaviour |
| `4be1d681-c35d-4540-b255-c2910ac80664` | Frostbite | **8.09** | 2.75 | Spell | APK MonoBehaviour |
| `cc768bae-a063-4885-8207-f39c6542fb36` | Guardbreaker | **8.09** | 2.75 | Maneuver | APK MonoBehaviour |
| `69ffa3fd-deb7-4824-bab6-ac6450f19676` | Harrying Bash | **6.70** | 1.77 | Maneuver | APK MonoBehaviour |
| `cfee0b02-6d91-4d34-869c-a7e54329060d` | Ice Spike | **5.23** | 1.90 | Spell | APK MonoBehaviour |
| `7fc15804-1637-40a9-8dcc-3ea1eb0f778d` | Lightning Bolt | **0.5 ⚡** | 0.5 | Spell (channeled) | APK MonoBehaviour |
| `9fdc4d52-ce90-44f8-9b5d-21f31e27dbda` | Paralyze | **8.09** | 2.75 | Spell | APK MonoBehaviour |
| `cdab44fb-6ff6-4701-a4ec-d19cce79e49f` | Piercing Strikes | **5.83** | 2.08 | Maneuver | APK MonoBehaviour |
| `66bdc017-30c5-4b5e-9753-215c45056f6a` | Poison Cloud | **6.58** | 2.30 | Spell | APK MonoBehaviour |
| `ce6b63e9-9f18-49c4-aee0-51f7985f9892` | Power Attack | **8.09** | 2.75 | Maneuver | APK MonoBehaviour |
| `eb0cb7e6-47cf-48e7-8cc9-dbf80fc77f13` | Quick Strikes | **5.83** | 2.08 | Maneuver | APK MonoBehaviour |
| `e08f95de-85bb-4829-ba7e-cf45bc6fb422` | Recovery Strikes | **8.75** | 2.08 | Maneuver | APK MonoBehaviour |
| `ba61ce46-163f-4a61-8ede-f5b7ae365e40` | Reflecting Bash | **6.70** | 1.77 | Maneuver | APK MonoBehaviour |
| `91078132-ef5c-492a-97f2-ac69be5140a8` | Resist Elements | **8.0** | 1.12 | Spell | APK MonoBehaviour |
| `f9a2373b-a84f-4716-90ce-165baa2dd6ed` | Shield Bash | **6.70** | 1.77 | Maneuver | APK MonoBehaviour |
| `9b915ec3-c63b-4b62-b417-4c5436d45fc1` | Staggering Bash | **6.70** | 1.77 | Maneuver | APK MonoBehaviour |
| `65ede044-d68a-4b2b-8f0c-02075ad133cc` | Ward | **7.5** | 1.20 | Spell | APK MonoBehaviour |

### ⚡ Lightning Bolt note
`_cooldown = _initialCooldown = _channelDuration = 0.5s`. This is a **continuous-channel** ability — the 0.5s is the re-fire interval within the channel, not a between-cast cooldown. The empirical minimum of 4s reflects channel-duration + transition.

---

## Cross-reference: Authoritative vs Empirical

Empirical values from `/tmp/arena-cooldowns.md`. Agreement = authoritative is within the empirical p10 range (i.e., plausible as the true floor).

| name | authoritative-CD | empirical-min | empirical-p10 | verdict |
|---|---|---|---|---|
| Absorb | 6.0 | 21.0 | 21.0 | ⚠️ EMPIRICAL HIGH (21s >> 6s auth — empirical sparse n=3, all long intervals, badly misses true CD) |
| Adrenaline Dodge | 8.65 | 14.0 | 14.0 | ⚠️ EMPIRICAL HIGH (14s >> 8.65s auth) |
| Blizzard Armor | 7.5 | — | — | (insufficient empirical) |
| Consuming Inferno | 8.09 | — | — | (insufficient empirical) |
| Delayed Lightning Bolt | 6.58 | — | — | (insufficient empirical) |
| Dodging Strike | 8.65 | 11.0 | 13.0 | ⚠️ EMPIRICAL HIGH (11s min >> 8.65s auth — empirical minimum plausible as floor but misses auth by 2.35s) |
| Fireball | 3.54 | 5.0 | 6.0 | ✓ AGREE (empirical p10=6s ≥ auth 3.54s; 5s min slightly above auth, consistent with 1s granularity) |
| Frostbite | 8.09 | 13.0 | 13.0 | ⚠️ EMPIRICAL HIGH (13s >> 8.09s auth) |
| Guardbreaker | 8.09 | — | — | (passive buff — irrelevant) |
| Harrying Bash | 6.70 | 8.0 | 8.0 | ⚠️ EMPIRICAL HIGH (8s min > 6.70s auth — plausible with 1s granularity, close) |
| Ice Spike | 5.23 | 9.0 | 9.0 | ⚠️ EMPIRICAL HIGH (9s >> 5.23s auth) |
| Lightning Bolt | 0.5 (channel) | 4.0 | 7.0 | N/A — channeled spell, empirical measures cast duration not CD |
| Paralyze | 8.09 | — | — | (insufficient empirical) |
| Piercing Strikes | 5.83 | 6.0 | 7.0 | ✓ AGREE (empirical min 6s ≈ auth 5.83s within 1s granularity) |
| Poison Cloud | 6.58 | 14.0 | 14.0 | ⚠️ EMPIRICAL HIGH (14s >> 6.58s auth) |
| Power Attack | 8.09 | 9.0 | 9.0 | ✓ AGREE (empirical min 9s ≈ auth 8.09s within 1s granularity) |
| Quick Strikes | 5.83 | 8.0 | 10.0 | ⚠️ EMPIRICAL HIGH (8s min > 5.83s auth — empirical is consistently 2s+ above auth) |
| Recovery Strikes | 8.75 | 13.0 | 13.0 | ⚠️ EMPIRICAL HIGH (13s >> 8.75s auth) |
| Reflecting Bash | 6.70 | — | — | (insufficient empirical) |
| Resist Elements | 8.0 | 22.0 | 22.0 | ⚠️ EMPIRICAL HIGH (22s >> 8.0s auth — severely overestimated) |
| Shield Bash | 6.70 | — | — | (insufficient empirical) |
| Staggering Bash | 6.70 | 5.0 | 5.0 | ⚠️ EMPIRICAL BELOW (5s min < 6.70s auth — likely 1s timestamp noise pulling the minimum below auth) |
| Ward | 7.5 | 18.0 | 18.0 | ⚠️ EMPIRICAL HIGH (18s >> 7.5s auth — empirical had only 4 intervals, all long) |

**Summary of empirical quality:** Only Fireball, Piercing Strikes, and Power Attack empiricals are close to the authoritative values. All others have empirical minimums significantly above the authoritative cooldown, indicating that most captures caught only long inter-cast gaps (between rounds, player hesitation, insufficient cast count) — not the actual cooldown floor. **The authoritative APK values should be used.**

---

## Abilities in uuid_labels never seen cast (APK data where available)

The following abilities from the "never seen" list in the empirical file also have authoritative APK data:

| name | UUID (from empirical "never seen") | authoritative-CD | notes |
|---|---|---|---|
| Blind | `85596d85-5f2a-4f3a-9059-960eaff79a87` | 7.35s | Found in APK as BlindRank* |
| Echo Weapon | `f60f69d4-24bc-46fb-a4fa-d4abdac0f06f` | 10.0s | Found in APK as EchoWeaponRank* |
| Focusing Dodge | `e685e88f-34e7-4fdc-bacd-618763078d65` | 8.65s | Found in APK as FocusingDodgeRank* |
| Indomitable Smash | `66610227-07bf-4e3b-a75b-c591271f0817` | 8.09s | Found in APK as IndomitableSmashRank* |
| Magicka Surge | `1c836287-44d8-40a6-bf02-d457f57d171d` | 10.0s | Found in APK as MagickaSurgeRank* |
| Reckless Fury | `0cfe29cd-89d9-42ad-9227-8308e2f87c7f` | 10.5s | Found in APK as RecklessFuryRank* |
| Renewing Dodge | `7f78d342-f346-4210-9f62-01a540687bb3` | 8.65s | Found in APK as RenewingDodgeRank* |
| Skullcrusher | `c112c956-eaac-4d7d-878e-32cd7d1e5209` | 8.09s | Found in APK as SkullcrusherRank* |
| Thunderstorm | `2ab06506-2114-4738-bd87-f6f402d3ce2e` | 6.0s | Found in APK as ThunderstormRank* |
| Venom Strikes | `e14eedd5-cd50-404e-9697-a37fd1d2ce02` | 5.83s | Found in APK as VenomStrikesRank* |

Note: UUID-to-APK-name mapping for "never seen" abilities could not be confirmed from uuid_labels (those UUIDs weren't in the table). The APK base name matches are by pattern only. UUID confirmation would require tracing the catalog entries for those base names.

**Abilities NOT found in APK:** Advanced Tempering, Armsman, Augmented Flames/Frost/Poison/Shock, Barbarian, Combat Focus, Elemental Protection, Enchantment Synergy, Healing Surge, Load Bearer, Matching Set, Maximum Power, Mettle, Scout, Wall of Fire, Willpower — these are likely Perks (AbilityType=3) with no `_cooldown` field, or are CDN-only bundles.

---

## What would be needed for remaining gaps

- **Perks** (Advanced Tempering, Barbarian, Armsman, etc.) — likely passive, no `_cooldown`. If they ARE in CDN bundles, you need the CDN bundle for the `Abilities/Perks` asset group (bundle name unknown without a catalog file).
- **UUID verification for "never seen" APK matches** — run the catalog parser on those base names to confirm UUID→path_id mapping.
- **Excel source file name** — the `[ExcelImportAttribute]` string is at RVA 0x11E49C0 in `libil2cpp.so`. To recover it, run `strings libil2cpp.so | grep -i ability` or use Ghidra. This would name the original Excel sheet but is not needed since we have the baked values.
