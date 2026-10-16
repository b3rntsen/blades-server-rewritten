# Economy logic audit (#392)

HauDrauf (#392): *"Build a system that monitors all shop items and chests until
everything runs right. Check everything for logic, not on the few data points."*

The loot bugs of the last days (#386 frozen captured jewellery, #392 arcane rings
from 32 recorded outcomes, #381 a 79%-Quicksilver Wooden chest, #509 Abyss loot
table) share one shape: a generator copied or sampled a **thin retail corpus**,
and the tests compared against that same corpus. This audit checks generated
output against what the **game's own tables** allow instead.

## How it works

`server/src/economy_audit.rs` (`cargo test -p server economy_audit`, ~25 s)
calls the real generators the handlers call — `global_shop::purchase_reward`,
`store_bundles::roll_bundle_with`, `chests::roll_loot` (treasury, daily),
`shop_gen::generate_catalog` + `jewelry_roll` (town merchants),
`global_shop::current_catalog` (storefront) — 500 times per subject and level,
with varied nonces and five buyer ids, at character levels
1, 10, 20, 30, 40, 45, 49, 60, 80 and 100.

| Check | What it holds every item / subject to | In CI |
|---|---|---|
| **Validity** | slot-valid GRADING; `grade == sum(tiers)` and a `JewelryGradeData` row; no property twice; ENCHANTING count 0 or arcane tier + 1; enchantments from the APK recipe or the template's secondary table, tier in the APK range; rings/necklaces graded; gear keeps durability; required level within what retail itself paid from that source | **asserted** |
| **Variety** | no rolled product/chest pays one item instance (template + all properties), one ring, or one whole reward in more than 25% of rolls (skipped where the APK definition is fixed) | **asserted** |
| **Coverage** | every GRADING property, weighted enchantment and loot-pool template the APK allows for that slot/tier is reached | warning |
| **Level fit** | no metal or enchanting ingredient above 60% of a chest's stacks where other kinds are paid at the level; at most 10% of gear above level + 2 | warning |
| **Availability** | per level, what the storefront lists after the client's APK level gate; offers never live, unpriced or ungrantable; caps that never reset | warning |

Known asserted failures are in `KNOWN_FAILURES` with a reason; CI stays green
and the report lists any entry that stopped firing ("remove the entry"), so the
list shrinks as fixes land. Regenerate the report with
`ECONOMY_AUDIT_WRITE=1 cargo test -p server economy_audit`; only the block
between the GENERATED markers is rewritten.

The APK facts the server does not load (gear `requiredLevel`, the storefront's
`_min/_maxPlayerLevelAllowedForPurchase`) are in
`server/data/economy_audit_apk.json`, written by
`script/extract_economy_audit_apk.py` from the APK (sha256 fd6e55f5…).

## First run, 2026-10-14

Every one of the **546 storefront products passes** (the Sigil shop after
#518/#524 included): fixed products are what their APK definition says, rolled
jewellery reaches all 34 ring and 15 necklace GRADING properties, nothing
invalid, nothing frozen. The defects are all in the **chests**.

### Defects, each fixed in its own PR

| # | Defect | Where | Numbers | Fix |
|---|---|---|---|---|
| 1 | **Chest rings and necklaces have no ability bonuses** (grade but no GRADING) | Silver, Gold, daily chests, every level | 77 of the 100 retail chest jewels — every enchanted one — lost GRADING in `extract_chest_loot.py`; up to 1 Silver/Gold item in 5 at a level is such a ring | #525 |
| 2 | **Treasury chest gear frozen** (one retail piece per slot, verbatim) | Wooden, Silver, Gold, daily | Wooden: the same Hide Gauntlets in 46/46 items at L1, Dwarven Plate Armor 43/43 at L49, Stalhrim Gauntlets 26/26 at L60; 26 instances in 1,088 items. Gold/Silver rings: one ring is 40-59% of rings at L30-49 | #527 (stacked on #525) |
| 3 | **Golden (Rare) store chest: APK-allowed bonuses unreachable** | Golden chest, every level | 8/34 ring and 2/15 necklace GRADING never appear; 552 pieces in 10,000; enchantments frozen | #526 |

Measured after each fix with this harness: (1) the "no GRADING" finding fires
0 times; (2) frozen items and repeating jewellery 0 times, Wooden 470 instances
in 1,088 items; (3) all 49 GRADING properties reached, 6,795 pieces in 10,000.
None of the fixes removes anything a player gets today: the retail piece stays
reachable at the same tier.

### Shop visibility at level ~46 ("many shop items no longer visible")

Not reproduced; no offer is hidden by a server rule, rotation window or cap bug.

- The client hides an offer outside its APK level gate. Over a whole 42-day
  cycle a level-46 character is listed **382 distinct offers** against 169 at
  level 44, and **35-45 a day (median 40)** against 25-32 at 40-44 and 29-39 at
  50+ — levels 45-49 see the most of any level (216 offers are gated 45-100).
  Only 4 offers drop out between 44 and 46: GemPack01_c, GemPack02_b,
  GemPack03_c_ExpertArchitect, GemPack05_b_JourneymanEnchanter, all gated to
  ≤45 by the APK.
- Prod, read-only (2026-10-14): Sephoris `f7817aa6` is level 47 with 38
  products bought and 89 cap keys. Of the 69 offers live that day, 39 are
  inside his gate; exactly **one** shows as sold out:
  SigilShop_Ultimate_FireResistHelmet, bought once against retail's own
  lifetime cap of 1 (`maxPurchases: 1`).
- Per-window caps reset: every windowed cap key moves with the replayed window
  (checked over two cycles), and the never-resetting `::override::` counters
  carry a limit only on lifetime-capped (Ultimate) offers.
- Three offers are never listed: retail scheduled them `isActive: false` in
  every window.

Asked back: which items (or which shop: Sigil shop, store, or a town merchant)
does he miss? A screenshot of the shop at 47 would settle it. Town merchants
stock by building level, not character level.

### Follow-ups (reported, not fixed)

- **Above-level gear in Wooden/Silver chests (narrowing → question).** The
  Wooden table is thin (44 openings with an item), so a level-10 Wooden chest's
  neighbourhood reaches openings at 20+: 45% of its items need more than level
  12, Elven gear (+13) included, beyond retail's own Wooden maximum (+10, the one
  allowlisted Validity finding). Silver at L10: 31%, Gold at L1: 19%. Pinning the
  items to the level would take away higher gear players get today.
- **Metal level fit (question).** Gold chests pay only Orichalcum at L20 (100%)
  and 77% at L10; Wooden 76% Quicksilver at L40 (#519 widened only from 41);
  Silver/daily 65-69% Malachite at 40-49 and 63-66% Stalhrim at 60-100; Gold
  63-70% Ebony at 40-45; Elder/Legendary 100% Dragon Bone from 45 (retail's
  54-89 band paid only that). These follow the retail openings near each level,
  not a thin table, so they are left alone unless HauDrauf wants the #519
  widening extended.
- **Coverage** (Elder/Legendary 20/21 templates per tier, Iron vs Steel at
  tier 1; Alchemist 10 of 62 measured bundles never stocked at building
  levels 0-9) — warnings only.
- The server does not enforce the APK level gate on purchase (the client does):
  informational, nothing to fix for players.

<!-- BEGIN GENERATED: cargo test -p server economy_audit (ECONOMY_AUDIT_WRITE=1) -->

Rolls per subject and level: **500**. Levels: 1, 10, 20, 30, 40, 45, 49, 60, 80, 100. Variety limit: 25% of rolls; level-fit limit: 60% of a family's stacks.

### Summary

| Check | Subjects flagged | Asserted |
|---|---:|---|
| Validity | 0 | yes (allowlist below) |
| Variety | 3 | yes (allowlist below) |
| Coverage | 11 | no, warning |
| Level fit | 10 | no, warning |
| Availability | 0 | no, warning |

549 of 562 subjects pass every check.

### Chests

| Subject | Generator | Levels | Rolls | Status | Findings | Notes |
|---|---|---|---:|---|---|---|
| Tutorial first chest (tier -1) | chests::roll_loot (POST /chests/{id}/collect) | 1 | 500 | PASS | - | scripted retail bundle (3 recorded openings): fixed by definition; 3/500 distinct rewards, 3 distinct item instances in 500 items |
| Wooden chest (tier 1) | chests::roll_loot (POST /chests/{id}/collect) | 1,10,20,30,40,45,49,60,80,100 | 500 | WARN | **Level fit**: IronBar is 79% of metal stacks at L1 (92/117; 6 kinds paid at this level)<br>**Level fit**: EbonyIngot is 61% of metal stacks at L40 (163/268; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 80% of metal stacks at L45 (115/143; 9 kinds paid at this level)<br>**Coverage**: tier 1 (req. level) gear: 24/42 APK loot-pool templates reached<br>**Coverage**: tier 8 (req. level) gear: 18/21 APK loot-pool templates reached<br>**Coverage**: tier 13 (req. level) gear: 19/21 APK loot-pool templates reached<br>**Coverage**: tier 18 (req. level) gear: 19/21 APK loot-pool templates reached<br>**Coverage**: tier 23 (req. level) gear: 19/21 APK loot-pool templates reached<br>**Coverage**: tier 28 (req. level) gear: 19/21 APK loot-pool templates reached<br>**Coverage**: tier 33 (req. level) gear: 19/21 APK loot-pool templates reached<br>**Coverage**: tier 39 (req. level) gear: 19/21 APK loot-pool templates reached<br>**Coverage**: tier 45 (req. level) gear: 17/21 APK loot-pool templates reached | 4688/5000 distinct rewards, 604 distinct item instances in 1088 items |
| Silver chest (tier 2) | chests::roll_loot (POST /chests/{id}/collect) | 1,10,20,30,40,45,49,60,80,100 | 500 | WARN | **Level fit**: MalachiteIngot is 75% of metal stacks at L30 (434/581; 9 kinds paid at this level)<br>**Level fit**: EbonyIngot is 73% of metal stacks at L40 (309/426; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 94% of metal stacks at L45 (256/273; 9 kinds paid at this level)<br>**Level fit**: MalachiteIngot is 69% of metal stacks at L49 (44/64; 8 kinds paid at this level)<br>**Level fit**: StalhrimIngot is 66% of metal stacks at L60 (79/120; 8 kinds paid at this level)<br>**Level fit**: StalhrimIngot is 63% of metal stacks at L100 (114/182; 8 kinds paid at this level)<br>**Coverage**: tier 1 (req. level) gear: 40/42 APK loot-pool templates reached<br>**Coverage**: tier 8 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 13 (req. level) gear: 19/21 APK loot-pool templates reached<br>**Coverage**: tier 23 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 28 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 33 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 39 (req. level) gear: 19/21 APK loot-pool templates reached<br>**Coverage**: tier 45 (req. level) gear: 19/21 APK loot-pool templates reached | 5000/5000 distinct rewards, 3378 distinct item instances in 5000 items |
| Gold chest (tier 3) | chests::roll_loot (POST /chests/{id}/collect) | 1,10,20,30,40,45,49,60,80,100 | 500 | WARN | **Level fit**: SteelIngot is 100% of metal stacks at L1 (92/92; 6 kinds paid at this level)<br>**Level fit**: SilverIngot is 100% of metal stacks at L10 (84/84; 11 kinds paid at this level)<br>**Level fit**: EbonyIngot is 94% of metal stacks at L40 (421/447; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 75% of metal stacks at L45 (187/250; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L80 (30/30; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L100 (35/35; 8 kinds paid at this level)<br>**Coverage**: tier 1 (req. level) gear: 41/42 APK loot-pool templates reached<br>**Coverage**: tier 13 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 23 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 39 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 45 (req. level) gear: 19/21 APK loot-pool templates reached | 5000/5000 distinct rewards, 7327 distinct item instances in 10005 items |
| Elder chest (tier 4) | chests::roll_loot (POST /chests/{id}/collect) | 1,10,20,30,40,45,49,60,80,100 | 500 | WARN | **Level fit**: SteelIngot is 100% of metal stacks at L1 (306/306; 6 kinds paid at this level)<br>**Level fit**: SilverIngot is 74% of metal stacks at L10 (260/351; 11 kinds paid at this level)<br>**Level fit**: EbonyIngot is 88% of metal stacks at L40 (488/557; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L45 (397/397; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L49 (40/40; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L60 (39/39; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L80 (49/49; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L100 (44/44; 8 kinds paid at this level)<br>**Coverage**: tier 1 (req. level) gear: 26/42 APK loot-pool templates reached<br>**Coverage**: tier 8 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 45 (req. level) gear: 20/21 APK loot-pool templates reached | 5000/5000 distinct rewards, 8452 distinct item instances in 10040 items |
| Legendary chest (tier 5) | chests::roll_loot (POST /chests/{id}/collect) | 1,10,20,30,40,45,49,60,80,100 | 500 | WARN | **Level fit**: SteelIngot is 100% of metal stacks at L1 (348/348; 6 kinds paid at this level)<br>**Level fit**: SilverIngot is 96% of metal stacks at L10 (281/293; 11 kinds paid at this level)<br>**Level fit**: EbonyIngot is 84% of metal stacks at L40 (498/594; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L45 (397/397; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L49 (45/45; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L60 (89/89; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L80 (97/97; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L100 (82/82; 8 kinds paid at this level)<br>**Coverage**: tier 1 (req. level) gear: 40/42 APK loot-pool templates reached | 5000/5000 distinct rewards, 9044 distinct item instances in 10237 items |
| Daily Silver chest | daily_reward::collect_reward_for -> chests::roll_loot | 1,10,20,30,40,45,49,60,80,100 | 500 | WARN | **Level fit**: MalachiteIngot is 74% of metal stacks at L30 (432/580; 9 kinds paid at this level)<br>**Level fit**: EbonyIngot is 74% of metal stacks at L40 (346/467; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 94% of metal stacks at L45 (240/254; 9 kinds paid at this level)<br>**Level fit**: MalachiteIngot is 67% of metal stacks at L49 (44/66; 8 kinds paid at this level)<br>**Level fit**: StalhrimIngot is 64% of metal stacks at L60 (76/119; 8 kinds paid at this level)<br>**Coverage**: tier 1 (req. level) gear: 40/42 APK loot-pool templates reached<br>**Coverage**: tier 8 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 13 (req. level) gear: 19/21 APK loot-pool templates reached<br>**Coverage**: tier 23 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 28 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 33 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 39 (req. level) gear: 19/21 APK loot-pool templates reached<br>**Coverage**: tier 45 (req. level) gear: 19/21 APK loot-pool templates reached | 5000/5000 distinct rewards, 3421 distinct item instances in 5000 items |
| Daily Gold chest | daily_reward::collect_reward_for -> chests::roll_loot | 1,10,20,30,40,45,49,60,80,100 | 500 | WARN | **Level fit**: SteelIngot is 100% of metal stacks at L1 (99/99; 6 kinds paid at this level)<br>**Level fit**: SilverIngot is 100% of metal stacks at L10 (82/82; 11 kinds paid at this level)<br>**Level fit**: MalachiteIngot is 61% of metal stacks at L30 (284/465; 9 kinds paid at this level)<br>**Level fit**: EbonyIngot is 96% of metal stacks at L40 (440/460; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 75% of metal stacks at L45 (193/257; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L80 (46/46; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L100 (42/42; 8 kinds paid at this level)<br>**Coverage**: tier 1 (req. level) gear: 41/42 APK loot-pool templates reached<br>**Coverage**: tier 13 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 23 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 39 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 45 (req. level) gear: 19/21 APK loot-pool templates reached | 5000/5000 distinct rewards, 7319 distinct item instances in 10003 items |
| Golden (Rare) store chest | store_bundles::roll_bundle_with (global-shop purchase) | 1,10,20,30,40,45,49,60,80,100 | 500 | WARN | **Level fit**: SteelIngot is 96% of metal stacks at L1 (134/139; 6 kinds paid at this level)<br>**Coverage**: tier 33 (req. level) gear: 20/21 APK loot-pool templates reached | 5000/5000 distinct rewards, 6812 distinct item instances in 10000 items |
| Epic (Elder) store chest | store_bundles::roll_bundle_with (global-shop purchase) | 1,10,20,30,40,45,49,60,80,100 | 500 | WARN | **Level fit**: SteelIngot is 87% of metal stacks at L1 (253/290; 6 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L45 (41/41; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L49 (50/50; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L60 (42/42; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L80 (50/50; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L100 (39/39; 8 kinds paid at this level)<br>**Coverage**: tier 1 (req. level) gear: 21/42 APK loot-pool templates reached<br>**Coverage**: tier 8 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 18 (req. level) gear: 20/21 APK loot-pool templates reached<br>**Coverage**: tier 45 (req. level) gear: 20/21 APK loot-pool templates reached | 5000/5000 distinct rewards, 8019 distinct item instances in 10055 items |
| Legendary store chest | store_bundles::roll_bundle_with (global-shop purchase) | 1,10,20,30,40,45,49,60,80,100 | 500 | WARN | **Level fit**: SteelIngot is 100% of metal stacks at L1 (290/290; 6 kinds paid at this level)<br>**Level fit**: SilverIngot is 61% of metal stacks at L10 (181/295; 11 kinds paid at this level)<br>**Level fit**: DwarvenIngot is 61% of metal stacks at L20 (292/475; 11 kinds paid at this level)<br>**Level fit**: MalachiteIngot is 68% of metal stacks at L30 (109/160; 9 kinds paid at this level)<br>**Level fit**: EbonyIngot is 61% of metal stacks at L40 (259/428; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L45 (40/40; 9 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L49 (45/45; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L60 (103/103; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L80 (89/89; 8 kinds paid at this level)<br>**Level fit**: DragonBone is 100% of metal stacks at L100 (89/89; 8 kinds paid at this level)<br>**Coverage**: tier 1 (req. level) gear: 40/42 APK loot-pool templates reached | 5000/5000 distinct rewards, 8917 distinct item instances in 10243 items |

### Town merchants

| Subject | Generator | Levels | Rolls | Status | Findings | Notes |
|---|---|---|---:|---|---|---|
| Blacksmith (Forge) | shop_gen::generate_catalog + jewelry_roll::roll_generated_jewelry | building 0-9 | 100 | FLAG | **Variety** (known): frozen catalog: building L5 lists the same 7 bundles in every window (pool 7 <= maxItems 7)<br>**Variety** (known): frozen catalog: building L6 lists the same 8 bundles in every window (pool 8 <= maxItems 8) | 87 bundles stocked, 0 jewellery rolls |
| Enchanter | shop_gen::generate_catalog + jewelry_roll::roll_generated_jewelry | building 0-9 | 100 | PASS | - | 35 bundles stocked, 895 jewellery rolls |
| Workshop | shop_gen::generate_catalog + jewelry_roll::roll_generated_jewelry | building 0-9 | 100 | FLAG | **Variety** (known): frozen catalog: building L2 lists the same 5 bundles in every window (pool 5 <= maxItems 5) | 34 bundles stocked, 0 jewellery rolls |
| Alchemist | shop_gen::generate_catalog + jewelry_roll::roll_generated_jewelry | building 0-9 | 100 | FLAG | **Coverage**: L0: 11/14 potions the APK sells at tiers 1-1 can never be stocked (Poison.DamageRestoration.Health.Tier1, Poison.DamageRestoration.Magicka.Tier1, Poison.DamageRestoration.Stamina.Tier1, Poison.ElementalWeakness.Fire.Tier1, ...)<br>**Variety** (known): frozen catalog: building L1 lists the same 5 bundles in every window (pool 5 <= maxItems 5)<br>**Coverage**: L1: 11/14 potions the APK sells at tiers 1-1 can never be stocked (Poison.DamageRestoration.Health.Tier1, Poison.DamageRestoration.Magicka.Tier1, Poison.DamageRestoration.Stamina.Tier1, Poison.ElementalWeakness.Fire.Tier1, ...)<br>**Coverage**: L2: 13/14 potions the APK sells at tiers 1-1 can never be stocked (Poison.DamageRestoration.Health.Tier1, Poison.DamageRestoration.Magicka.Tier1, Poison.DamageRestoration.Stamina.Tier1, Poison.ElementalWeakness.Fire.Tier1, ...)<br>**Coverage**: L3: 13/14 potions the APK sells at tiers 2-2 can never be stocked (Poison.DamageRestoration.Health.Tier2, Poison.DamageRestoration.Magicka.Tier2, Poison.DamageRestoration.Stamina.Tier2, Poison.ElementalWeakness.Fire.Tier2, ...)<br>**Coverage**: L4: 24/28 potions the APK sells at tiers 2-3 can never be stocked (Poison.DamageRestoration.Health.Tier2, Poison.DamageRestoration.Magicka.Tier3, Poison.DamageRestoration.Stamina.Tier2, Poison.DamageRestoration.Stamina.Tier3, ...)<br>**Coverage**: L5: 12/14 potions the APK sells at tiers 4-4 can never be stocked (Poison.DamageRestoration.Health.Tier4, Poison.DamageRestoration.Magicka.Tier4, Poison.DamageRestoration.Stamina.Tier4, Poison.ElementalWeakness.Fire.Tier4, ...)<br>**Variety** (known): frozen catalog: building L6 lists the same 7 bundles in every window (pool 7 <= maxItems 7)<br>**Coverage**: L6: 13/14 potions the APK sells at tiers 5-5 can never be stocked (Poison.DamageRestoration.Health.Tier5, Poison.DamageRestoration.Magicka.Tier5, Poison.DamageRestoration.Stamina.Tier5, Poison.ElementalWeakness.Fire.Tier5, ...)<br>**Coverage**: L7: 11/14 potions the APK sells at tiers 6-6 can never be stocked (Poison.DamageRestoration.Health.Tier6, Poison.DamageRestoration.Magicka.Tier6, Poison.DamageRestoration.Stamina.Tier6, Poison.ElementalWeakness.Fire.Tier6, ...)<br>**Coverage**: L8: 24/28 potions the APK sells at tiers 7-8 can never be stocked (Poison.DamageRestoration.Health.Tier7, Poison.DamageRestoration.Health.Tier8, Poison.DamageRestoration.Magicka.Tier7, Poison.DamageRestoration.Magicka.Tier8, ...)<br>**Coverage**: 12/73 stockable bundles never stocked at building levels 0-9 | 61 bundles stocked, 0 jewellery rolls |

### Storefront availability

Offers the client lists at each character level (live in the replayed calendar AND inside the APK `_min/_maxPlayerLevelAllowedForPurchase` gate), one sample a day over a whole 42-day cycle from 2026-10-15. 543 distinct offers are live at some point.

| Level | min | median | max | mean |
|---:|---:|---:|---:|---:|
| 1 | 13 | 17 | 20 | 16 |
| 10 | 16 | 19 | 23 | 19 |
| 20 | 20 | 25 | 29 | 24 |
| 30 | 23 | 27 | 31 | 26 |
| 35 | 24 | 28 | 32 | 27 |
| 40 | 25 | 29 | 32 | 29 |
| 44 | 25 | 29 | 32 | 28 |
| 45 | 35 | 40 | 45 | 40 |
| 46 | 35 | 40 | 45 | 39 |
| 47 | 35 | 40 | 45 | 39 |
| 48 | 35 | 40 | 45 | 39 |
| 49 | 35 | 40 | 45 | 39 |
| 50 | 30 | 35 | 39 | 34 |
| 60 | 29 | 34 | 38 | 33 |
| 80 | 29 | 34 | 37 | 33 |
| 100 | 29 | 34 | 37 | 33 |

| Subject | Generator | Levels | Rolls | Status | Findings | Notes |
|---|---|---|---:|---|---|---|
| Storefront over one 42-day replay cycle | global_shop::current_catalog + APK level gate (client filter) | 1,10,20,30,35,40,44,45,46,47,48,49,50,60,80,100 | 42 | PASS | - | 3 offers inactive in every retail window (never shown by retail either): 0413eb45, 88589dc7, 9440d12e; 540/543 served offers have an APK entry (the rest are ungated) |

### Global-shop products (Sigil shop and storefront)

<details><summary>546 products PASS every check</summary>

| Product | Generator | Notes |
|---|---|---|
| SigilShop_DivineIronLightHammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_DaedricLightShield_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_Witsplinter | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells Witsplinter (requires 45); authored that way; 1 distinct item instances |
| GemPack03_b_JourneyManArchitect | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_EffectIngredients_GlowDust | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineEbonyDagger | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineStalhrimArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_EbonyHelmet_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DwarvenMace_Temp08_MgcDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSteelArmor | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenPlateArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| 0413eb45 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 2 distinct item instances |
| SigilShop_Armor_DwarvenMailGauntlets_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DwarvenBattleaxe_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineEbonyGreatsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldRubyRing_PowerAttackQuickStrikes_NoEnch | global_shop::purchase_reward, jewellery rolled per purchase | 483 distinct item instances |
| SigilShop_Shield_DaedricLightShield_Arcane01 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_ChitinGauntlets_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells ChitinGauntlets (requires 8); authored that way; 1 distinct item instances |
| SigilShop_DivineQuicksilverHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSilverBoots | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| GemPack02_b | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Weapon_ElvenMace_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineOrcishScaleHelmet | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_QuicksilverShield_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DragonboneDagger_Temp04_NoEnch | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineEbonyLongsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineGlassArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DwarvenWarhammer_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| IAP_HealingPack_04 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_LTUltimate_DefensiveSpellsRing | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 25-55 sells LTUltimate.DefensiveSpellsRing (requires 30); authored that way; 1 distinct item instances |
| SigilShop_Ultimate_FireWarhammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells FireWarhammer (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Weapon_DwarvenGreatsword_Temp04_PhyBlockPiercing | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineGlassGauntlets | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_SilverLightHammer_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverLightHammer (requires 8); authored that way; 1 distinct item instances |
| SigilShop_Weapon_DragonboneHandAxe_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_EbonyDiamondRing_FireballConsumingInfernoWallOfFire_NoEnch | global_shop::purchase_reward, jewellery rolled per purchase | 472 distinct item instances |
| SigilShop_DivineDwarvenHandAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_QuicksilverHelmet_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_ChaurusGauntlets_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_EffectIngredients_Deathbell | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineSilverArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| DecoOffer_M_FighterTrophy_2019-06-17 00:00_2019-06-23 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Armor_QuicksilverGauntlets_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| EmoteOffer_Salute_2019-12-05 00:00_2019-12-08 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Weapon_EbonyMace_Temp08_Haste | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DaedricGreatsword_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_EbonyAtroniteRing_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 474 distinct item instances |
| 11102495 | randomised store chest | rolled by store_bundles::roll_bundle_with; audited in the chest section |
| SigilShop_Smithing_T2-T5 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_TownMaterials_Limestone_x50 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Weapon_SilverMace_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverMace (requires 8); authored that way; 1 distinct item instances |
| 11b322d2 | randomised store chest | rolled by store_bundles::roll_bundle_with; audited in the chest section |
| SigilShop_Armor_StalhrimGauntlets_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_EbonyWarhammer_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DragonboneMace_Arcane02 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineStalhrimBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricHandAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_OrcishScaledShield_Temp08_PowBlock | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineElvenBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DwarvenMailArmor_Temp08_FortStm | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DwarvenPlateArmor_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DwarvenPlateHelmet_Temp04_FortHp | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_HealthHelmet | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells HealthHelmet (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineGlassWarAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_OrcishHandAxe_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSilverBattleaxe | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineEbonyWarAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldRubyNecklace_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 421 distinct item instances |
| SigilShop_DivineDwarvenPlateGauntlets | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DragonplateBoots_Temp08_FortStm | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineLeatherHelmet | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_GlassShield_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_ElvenWarhammer_Temp06_MgcDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_SilverBattleaxe_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverBattleaxe (requires 8); authored that way; 1 distinct item instances |
| IAP_HealingPack_01 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Jewelry_GoldSapphireRing_FrostbiteIceSpike_NoEnch | global_shop::purchase_reward, jewellery rolled per purchase | 472 distinct item instances |
| SigilShop_Jewel_Atronite | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_SoulGem_Grand | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_LTUltimate_PoisonLongsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 25-55 sells LTUltimate.PoisonLongsword (requires 30); authored that way; 1 distinct item instances |
| SigilShop_Armor_DaedricMailGauntlets_Temp06_NoEnch | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| DecoOffer_M_Stele1_2019-02-10 00:00_2019-02-13 23:59 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Weapon_DragonboneGreatsword_Arcane02 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_MagickaGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells MagickaGauntlets (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Jewelry_EbonyDiamondRing_PowerAttackDodgingStrikeQuickStrikes_AbsStm | global_shop::purchase_reward, jewellery rolled per purchase | 500 distinct item instances |
| SigilShop_Weapon_DragonboneBattleaxe_Arcane02 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| EmoteOffer_Guffaw_2019-12-14 00:00_2019-12-15 23:59 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Weapon_DaedricMace_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineQuicksilverGauntlets | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_SoulGem_Transcendent | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineElvenDagger | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineElvenHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| EmoteOffer_FistFight_2019-12-05 00:00_2019-12-08 23:57 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineDaedricDagger | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineOrcishWarAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineGlassShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| EmoteOffer_YouAreNext_2019-12-09 00:00_2019-12-13 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Shield_EbonyShield_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_OrcishScaleHelmet_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineElvenMace | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_DragonplateShield_Temp06_NoEnch | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenDagger | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_SilverHelmet_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverHelmet (requires 8); authored that way; 1 distinct item instances |
| SigilShop_Weapon_DaedricWarAxe_Temp04_MgcDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_ElvenBoots_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_EbonyShield_Arcane02 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineLeatherBoots | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewel_Diamond | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Jewelry_GoldSapphireRing_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 469 distinct item instances |
| SigilShop_DivineOrcishScaleBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSilverGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricPlateBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineChaurusShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSilverWarhammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldRubyRing_LightningBoltDelayedLightningBolt_AbsMgc | global_shop::purchase_reward, jewellery rolled per purchase | 499 distinct item instances |
| SigilShop_Jewelry_EbonyAtroniteRing_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | 474 distinct item instances |
| SigilShop_DivineIronBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_DragonscaleShield_Temp08_PowBlock | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_EbonyDiamondNecklace_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 427 distinct item instances |
| SigilShop_Weapon_OrcishBattleaxe_Temp08_StmDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricMailHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineElvenGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_EbonyAtroniteRing_ShieldBashDodgingStrikeQuickStrikes_NoEnch | global_shop::purchase_reward, jewellery rolled per purchase | 477 distinct item instances |
| SigilShop_Jewel_Topaz | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Jewelry_SilverAmethystNecklace_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | APK gate 1-19 sells Jewelry.Necklace.SilverAmethyst (requires 13); authored that way; 431 distinct item instances |
| SigilShop_DivineIronHandAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_OrcishWarhammer_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Smithing_T6-T7 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Armor_DaedricMailHelmet_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_EffectIngredients_AllIngredients | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineChitinHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldSapphireNecklace_Barbarian_FortStmRegen | global_shop::purchase_reward, jewellery rolled per purchase | 499 distinct item instances |
| SigilShop_DivineOrcishScaledShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DragonboneDagger_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_EffectIngredients_FrostSalts | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineGlassHandAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_SilverLongsword_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverLongsword (requires 8); authored that way; 1 distinct item instances |
| SigilShop_Weapon_EbonyWarAxe_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_Serpentstrike | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells Serpenstrike (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineDaedricLightShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_EbonyGreatsword_Temp04_StmDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_OffensiveSpellsRing | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells OffensiveSpellsRing (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Shield_DaedricShield_Temp04_PowBlock | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSilverLongsword | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_LTUltimate_FireHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 25-55 sells LTUltimate.FireResistHelmet (requires 30); authored that way; 1 distinct item instances |
| SigilShop_DivineDwarvenLongsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldEmeraldNecklace_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 424 distinct item instances |
| SigilShop_DivineQuicksilverArmor | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenMailBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_FrostWarAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells FrostWarAxe (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineDaedricWarhammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_LTUltimate_HealthArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 25-55 sells LTUltimate.HealthArmor (requires 30); authored that way; 1 distinct item instances |
| SigilShop_DivineSteelDagger | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineElvenArmor | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_CaptainKordansSaber | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells CaptainKordansSaber (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineSilverLightHammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_SilverAmethystRing_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | APK gate 1-19 sells Jewelry.Ring.SilverAmethyst (requires 13); authored that way; 475 distinct item instances |
| SigilShop_SoulGem_Glorious | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Shield_ChaurusShield_Temp08_PowBlock | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_ChaurusHelmet_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_EbonyDiamondRing_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | 472 distinct item instances |
| SigilShop_DivineHideArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineGlassBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineStalhrimGauntlets | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineElvenLightHammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_PoisonBattleaxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells PoisonBattleaxe (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineOrcishMace | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineIronMace | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSteelWarhammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_EbonyGauntlets_Temp04_AbsHp | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineStalhrimShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldEmeraldRing_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 479 distinct item instances |
| SigilShop_DivineEbonyBattleaxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSilverHandAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSteelShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack04_d_HeroicBlacksmith | global_shop::purchase_reward, captured retail grant (fixed) |  |
| IAP_ElementalPack_03 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_DivineSteelHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_EbonyFaeriteRing_PowerAttackDodgingStrikeQuickStrikes_AbsStm | global_shop::purchase_reward, jewellery rolled per purchase | 500 distinct item instances |
| SigilShop_Jewel_Faerite | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Shield_OrcishShield_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_EbonyAtroniteNecklace_Barbarian_FortStmRegen | global_shop::purchase_reward, jewellery rolled per purchase | 500 distinct item instances |
| SigilShop_Armor_OrcishPlateArmor_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Gold_30k | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_DivineSilverHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldSapphireRing_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | 469 distinct item instances |
| SigilShop_Jewelry_EbonyAtroniteRing_LightningBoltDelayedLightningBoltThunderstorm_AbsMgc | global_shop::purchase_reward, jewellery rolled per purchase | 498 distinct item instances |
| SigilShop_DivineOrcishGreatsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineOrcishWarhammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewel_Garnet | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Weapon_GlassGreatsword_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_TownMaterials_Copper_x40 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_EffectIngredients_FireSalts | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Armor_DwarvenPlateBoots_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DragonboneLightHammer_Arcane01 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineChitinShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_RingOfShock | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells RingOfShock (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineElvenWarAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| DecoOffer_S_BundleBannerMarkarth_2019-08-13 00:00_2019-08-16 23:59 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_DivineDwarvenLightShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineGlassHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_GlassBoots_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineGlassDagger | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineOrcishShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DwarvenLongsword_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldRubyNecklace_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | 421 distinct item instances |
| SigilShop_Jewelry_GoldEmeraldNecklace_Armsman_NoEnch | global_shop::purchase_reward, jewellery rolled per purchase | 422 distinct item instances |
| SigilShop_DivineIronGauntlets | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_OrcishScaleBoots_Temp06_FortMgc | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSteelWarAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_SilverWarAxe_Temp04_StmDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverWarAxe (requires 8); authored that way; 1 distinct item instances |
| SigilShop_DivineEbonyShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_GlassArmor_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DragonscaleBoots_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_GlassMace_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DaedricPlateArmor_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineChitinBoots | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_EbonyFaeriteNecklace_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | 425 distinct item instances |
| GemPack05_c_ExpertEnchanter | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Ultimate_ShockResistBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells ShockResistBoots (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineStalhrimHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_LTUltimate_FireRing | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 25-55 sells LTUltimate.FireRing (requires 30); authored that way; 1 distinct item instances |
| SigilShop_TownMaterials_MarbleBronze_x50 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_LTUltimate_ShockBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 25-55 sells LTUltimate.ShockResistBoots (requires 30); authored that way; 1 distinct item instances |
| SigilShop_DivineDaedricGreatsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenLightHammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DaedricPlateHelmet_Temp06_FortMgc | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DragonplateGauntlets_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenMailGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack05_d_HeroicEnchanter | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineIronWarhammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_GlassLongsword_Temp06_StmDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DwarvenDagger_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack01_c | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_SoulGem_Bundle_Glorious | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_SoulGem_Bundle_Grand | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Armor_DwarvenMailHelmet_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSilverWarAxe | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_StalhrimArmor_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_SoulGem_Bundle_Middling | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineIronBattleaxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_GlassWarhammer_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricPlateGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack04_a_ApprenticeBlacksmith | global_shop::purchase_reward, captured retail grant (fixed) |  |
| EmoteOffer_Arrow_2019-12-28 00:00_2019-12-29 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Ultimate_PurifiersCocoon | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells PurifiersCocoon (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Weapon_GlassLightHammer_Temp04_Combo | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack03_a_ApprenticeArchitect | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineIronArmor | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DaedricWarhammer_Temp06_PhyBlockPiercing | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_EbonyFaeriteNecklace_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 425 distinct item instances |
| SigilShop_DivineGlassLightHammer | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| EmoteOffer_FireBall_2019-12-23 00:00_2019-12-27 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Ultimate_HealthArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells HealthArmor (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineOrcishPlateBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack01_b | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Weapon_DragonboneLightHammer_Temp04_StmDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_ElvenDagger_Temp08_StmDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_ElvenHandAxe_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_LTUltimate_AkaviriKatana | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 25-55 sells LTUltimate.AkaviriKatana (requires 30); authored that way; 1 distinct item instances |
| SigilShop_TownMaterials_Lumber_x120 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_DivineLeatherArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_SoulGem_Bundle_Elevated | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineChaurusGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DragonscaleGauntlets_Temp06_AbsMgc | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_TownMaterials_Copper_x160 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Jewelry_EbonyFaeriteRing_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 480 distinct item instances |
| SigilShop_DivineSteelBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_DwarvenLightShield_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_TownMaterials_Lumber_x60 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_DivineDaedricLightHammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_GlassWarAxe_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_SoulGem_Bundle_Greater | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Jewel_Sapphire | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_LTUltimate_MagickaArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 25-55 sells LTUltimate.MagickaArmor (requires 30); authored that way; 1 distinct item instances |
| SigilShop_Weapon_OrcishGreatsword_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| DecoOffer_S_BundleBannerMorthal_2019-06-10 00:00_2019-06-12 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Weapon_DwarvenHandAxe_Temp06_Combo | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineIronShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenWarhammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineElvenGreatsword | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineElvenHandAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_GlassHandAxe_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_StalhrimShield_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| 88589dc7 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 2 distinct item instances |
| SigilShop_Armor_GlassGauntlets_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_DwarvenShield_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineEbonyLightHammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineOrcishDagger | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineOrcishPlateArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_ElvenLightHammer_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineGlassMace | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineHideGauntlets | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_SoulGem_Bundle_Common | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_DivineOrcishLongsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_SilverBoots_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverBoots (requires 8); authored that way; 1 distinct item instances |
| SigilShop_Weapon_DragonboneWarhammer_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineEbonyBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack02_a | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Shield_ChitinShield_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells ChitinShield (requires 8); authored that way; 1 distinct item instances |
| SigilShop_Jewel_Pearl | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Armor_DragonplateArmor_Arcane01 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_TownMaterials_Limestone_x100 | global_shop::purchase_reward, APK offer contents |  |
| GemPack03_c_ExpertArchitect | global_shop::purchase_reward, captured retail grant (fixed) |  |
| IAP_ElementalPack_01 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Jewelry_SilverAmethystNecklace_Armsman_FortMgcRegen | global_shop::purchase_reward, jewellery rolled per purchase | APK gate 1-19 sells Jewelry.Necklace.SilverAmethyst (requires 13); authored that way; 499 distinct item instances |
| SigilShop_DivineGlassWarhammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_SoulGem_Bundle_Transcendent | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_TownMaterials_Limestone_x200 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Smithing_T10 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_DivineGlassBattleaxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSteelLightHammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_QuicksilverShield_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| 9440d12e | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 2 distinct item instances |
| SigilShop_Jewel_Emerald | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_DivineIronHelmet | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_FireResistHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells FireResistHelmet (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Armor_DragonscaleGauntlets_Arcane01 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DragonboneWarhammer_Arcane01 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_OrcishScaleArmor_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_ShieldOfChorrol | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells ShieldOfChorrol (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Jewel_Amethyst | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Armor_DaedricPlateGauntlets_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DragonscaleHelmet_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_LTUltimate_ShockMace | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 25-55 sells LTUltimate.ShockMace (requires 30); authored that way; 1 distinct item instances |
| SigilShop_Shield_OrcishShield_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_GlassShield_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_ChitinHelmet_Temp04_FortHp | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells ChitinHelmet (requires 8); authored that way; 1 distinct item instances |
| SigilShop_Weapon_ElvenWarAxe_Temp04_Haste | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricPlateHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| DecoOffer_S_Nirnroot_2019-02-02 00:00_2019-02-05 23:59 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_LTUltimate_FireLongsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 25-55 sells LTUltimate.FireLongsword (requires 30); authored that way; 1 distinct item instances |
| SigilShop_DivineDaedricMailArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_OrcishDagger_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| 9e4dc391 | randomised store chest | rolled by store_bundles::roll_bundle_with; audited in the chest section |
| SigilShop_Jewelry_EbonyAtroniteNecklace_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 423 distinct item instances |
| SigilShop_Gold_20k | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineIronLongsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_TownMaterials_Limestone_x25 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Shield_DragonscaleShield_Arcane01 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineElvenLongsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSteelGreatsword | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_SilverShield_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-15 sells SilverShield (requires 8); authored that way; 1 distinct item instances |
| SigilShop_DivineDwarvenBattleaxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_ElvenShield_Temp04_PowBlock | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DaedricMailBoots_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_ElvenGauntlets_Temp04_AbsHp | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricMailGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_RingOfCinders | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells RingOfCinders (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Jewelry_GoldEmeraldRing_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | 479 distinct item instances |
| SigilShop_Jewelry_GoldSapphireNecklace_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | 438 distinct item instances |
| SigilShop_DivineLeatherShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_LTUltimate_OffensiveSpellsRing | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 25-55 sells LTUltimate.OffensiveSpellsRing (requires 30); authored that way; 1 distinct item instances |
| SigilShop_DivineChaurusHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_TownMaterials_Lumber_x240 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Jewelry_SilverAmethystRing_PowerAttackDodgingStrike_AbsStm | global_shop::purchase_reward, jewellery rolled per purchase | APK gate 1-19 sells Jewelry.Ring.SilverAmethyst (requires 13); authored that way; 499 distinct item instances |
| SigilShop_Jewelry_GoldEmeraldRing_PoisonCloudBlind_AbsMgc | global_shop::purchase_reward, jewellery rolled per purchase | 500 distinct item instances |
| SigilShop_Gold_55k | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_TownMaterials_Lumber_x30 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineOrcishScaleGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| DecoOffer_M_Imperial_Bath_2019-09-06 00:00_2019-09-09 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Shield_DragonplateShield_Arcane02 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_ChaurusBoots_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_ElvenGreatsword_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineOrcishLightHammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewel_FirebreathOpal | global_shop::purchase_reward, APK offer contents |  |
| DecoOffer_L_UnicornFountain_2019-05-01 00:00_2019-05-07 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineEbonyHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Gold_45k | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Weapon_EbonyLightHammer_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldRubyNecklace_Scout_NoEnch | global_shop::purchase_reward, jewellery rolled per purchase | 419 distinct item instances |
| SigilShop_DivineEbonyWarhammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSteelMace | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineIronDagger | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack01_a | global_shop::purchase_reward, captured retail grant (fixed) |  |
| EmoteOffer_NirnRoot_2019-12-21 00:00_2019-12-22 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineSilverGreatsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricLongsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldRubyRing_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | 471 distinct item instances |
| SigilShop_Ultimate_AkaviriSunderblade | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells AkaviriSunderblade (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Jewelry_EbonyDiamondNecklace_Scout_FortMgcRegen | global_shop::purchase_reward, jewellery rolled per purchase | 494 distinct item instances |
| DecoOffer_M_FullSpire_2019-05-17 00:00_2019-05-20 23:59 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Ultimate_RingOfFrost | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells RingOfFrost (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Ultimate_StaminaShield | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells StaminaShield (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Armor_DaedricMailArmor_Temp04_FortHp | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_EbonyLongsword_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| DecoOffer_M_Bones_2019-02-18 00:00_2019-02-21 23:59 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Jewelry_EbonyAtroniteNecklace_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | 423 distinct item instances |
| SigilShop_EffectIngredients_Honeycomb | global_shop::purchase_reward, captured retail grant (fixed) |  |
| GemPack05_a_ApprenticeEnchanter | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_DivineDwarvenShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_SoulGem_Bundle_Exceptional | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Armor_EbonyBoots_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DwarvenPlateGauntlets_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_SilverHandAxe_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverHandAxe (requires 8); authored that way; 1 distinct item instances |
| SigilShop_Armor_StalhrimBoots_Temp08_FortStm | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack03_d_HeroicAchitect | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_LTUltimate_StaminaShield | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 25-55 sells LTUltimate.StaminaShield (requires 30); authored that way; 1 distinct item instances |
| SigilShop_Jewelry_SilverAmethystRing_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | APK gate 1-19 sells Jewelry.Ring.SilverAmethyst (requires 13); authored that way; 475 distinct item instances |
| SigilShop_Jewelry_EbonyDiamondRing_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 472 distinct item instances |
| SigilShop_Shield_DaedricShield_Arcane02 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_EbonyDagger_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DragonboneBattleaxe_Temp08_MgcDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_TownMaterials_Copper_x80 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_DivineElvenWarhammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_EbonyBattleaxe_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_StaminaGauntlets | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells StaminaGauntlets (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineHideHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_ShockMace | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells ShockMace (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineQuicksilverBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineElvenShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DragonboneWarAxe_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_ChitinBoots_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells ChitinBoots (requires 8); authored that way; 1 distinct item instances |
| SigilShop_DivineGlassGreatsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack02_c | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Shield_SilverShield_Temp04_PowBlock | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverShield (requires 8); authored that way; 1 distinct item instances |
| SigilShop_DivineDwarvenMace | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineChaurusBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineOrcishPlateGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_ElvenShield_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_Breathtaker | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells BreathTaker (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineSilverMace | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_FireLongsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 35-100 sells FireLongsword (requires 45); authored that way; 1 distinct item instances |
| GemPack01_d | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Weapon_DaedricHandAxe_Temp08_NoEnch | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DragonplateArmor_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineOrcishBattleaxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_OrcishPlateGauntlets_Temp08_AbsStm | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenWarAxe | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldEmeraldNecklace_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | 424 distinct item instances |
| SigilShop_Armor_StalhrimHelmet_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenGreatsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_SilverAmethystRing_FireballConsumingInferno_NoEnch | global_shop::purchase_reward, jewellery rolled per purchase | APK gate 1-19 sells Jewelry.Ring.SilverAmethyst (requires 13); authored that way; 460 distinct item instances |
| SigilShop_DivineHideBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack04_b_JourneymanBlacksmith | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Weapon_SilverGreatsword_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverGreatsword (requires 8); authored that way; 1 distinct item instances |
| SigilShop_DivineElvenBattleaxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_EbonyArmor_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_DwarvenLightShield_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenMailHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricMace | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_MephalasTeacher | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells MephalasTeacher (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineChitinGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenPlateBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_OrcishPlateHelmet_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_SoulGem_Petty | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Jewelry_GoldEmeraldRing_ShieldBashQuickStrikes_NoEnch | global_shop::purchase_reward, jewellery rolled per purchase | 478 distinct item instances |
| SigilShop_Armor_SilverArmor_Temp06_FortMgc | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverArmor (requires 8); authored that way; 1 distinct item instances |
| SigilShop_Armor_OrcishScaleGauntlets_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_TownMaterials_Copper_x20 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_EffectIngredients_VoidSalts | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_LTUltimate_FireWarhammer | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 25-55 sells LTUltimate.FireWarhammer (requires 30); authored that way; 1 distinct item instances |
| SigilShop_Weapon_DwarvenWarAxe_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineGlassLongsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricBattleaxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_SilverWarhammer_Temp06_PhyBlockPiercing | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverWarhammer (requires 8); authored that way; 1 distinct item instances |
| SigilShop_DivineHideShield | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DragonscaleArmor_Arcane02 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_QuicksilverArmor_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineOrcishPlateHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_GlassDagger_Temp04_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenMailArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricPlateArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_EbonyFaeriteRing_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | 480 distinct item instances |
| SigilShop_DivineEbonyHandAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewel_Pyrine | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Ultimate_MagickaArmor | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells MagickaArmor (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Armor_DragonplateBoots_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSteelGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_LTUltimate_FrostWarAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 25-55 sells LTUltimate.FrostWarAxe (requires 30); authored that way; 1 distinct item instances |
| SigilShop_Armor_ElvenHelmet_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DaedricPlateBoots_Temp08_NoEnch | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| IAP_HealingPack_02 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_DivineDaedricWarAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_Stormkiss | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells Stormkiss (requires 45); authored that way; 1 distinct item instances |
| SigilShop_Jewelry_EbonyFaeriteRing_FrostbiteIceSpikeBlizzardArmor_NoEnch | global_shop::purchase_reward, jewellery rolled per purchase | 477 distinct item instances |
| SigilShop_Shield_StalhrimShield_Temp06_PowBlock | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_ElvenBattleaxe_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_QuicksilverBoots_Temp06_FortMgc | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| IAP_ElementalPack_02 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Shield_ChitinShield_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-15 sells ChitinShield (requires 8); authored that way; 1 distinct item instances |
| SigilShop_Shield_OrcishScaledShield_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDaedricMailBoots | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_ChaurusArmor_Temp08_FortStm | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DaedricBattleaxe_Arcane01 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_ElvenLongsword_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewel_Ruby | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Jewel_Arentine | global_shop::purchase_reward, APK offer contents |  |
| EmoteOffer_BabyDrake_2020-01-18 00:00_2020-01-19 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Weapon_OrcishWarAxe_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Smithing_T8-T9 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Weapon_OrcishLongsword_Temp06_Haste | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineEbonyArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Ultimate_DefensiveSpellsRing | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; APK gate 35-100 sells DefensiveSpellsRing (requires 45); authored that way; 1 distinct item instances |
| SigilShop_DivineChitinArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| IAP_HealingPack_03 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_Armor_ChitinArmor_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells ChitinArmor (requires 8); authored that way; 1 distinct item instances |
| EmoteOffer_Tantrum_2019-12-16 00:00_2019-12-20 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Armor_SilverGauntlets_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverGauntlets (requires 8); authored that way; 1 distinct item instances |
| SigilShop_DivineSteelBattleaxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_SilverDagger_Temp08_Combo | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; APK gate 1-14 sells SilverDagger (requires 8); authored that way; 1 distinct item instances |
| SigilShop_Weapon_DragonboneLongsword_Temp06_Haste | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_OrcishLightHammer_Temp04_MgcDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldRubyRing_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 471 distinct item instances |
| SigilShop_DivineOrcishScaleArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| DecoOffer_S_Stele3_2018-02-03 00:00_2018-02-05 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Armor_DwarvenMailBoots_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DragonplateHelmet_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Smithing_T1-T3 | global_shop::purchase_reward, APK offer contents |  |
| SigilShop_SoulGem_Bundle_Lesser | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Weapon_OrcishMace_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineQuicksilverShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineOrcishHandAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_DragonscaleHelmet_Temp08_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSilverShield | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| GemPack05_b_JourneymanEnchanter | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Jewelry_SilverAmethystNecklace_Arcane01 | global_shop::purchase_reward, jewellery rolled per purchase | APK gate 1-19 sells Jewelry.Necklace.SilverAmethyst (requires 13); authored that way; 431 distinct item instances |
| SigilShop_Weapon_DwarvenLightHammer_Temp06_NoEnch | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineEbonyGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineChaurusArmor | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DragonboneWarAxe_Temp06_NoEnch | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldSapphireRing_ShieldBashDodgingStrike_AbsStm | global_shop::purchase_reward, jewellery rolled per purchase | 498 distinct item instances |
| SigilShop_Weapon_GlassBattleaxe_Temp08_PhyBlockPiercing | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineDwarvenPlateHelmet | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_ChaurusShield_Arcane02 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSteelLongsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DaedricLongsword_Arcane02 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| GemPack04_c_ExpertBlacksmith | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Weapon_DaedricLightHammer_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DragonboneLongsword_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Shield_DwarvenShield_Temp06_PowBlock | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Jewelry_GoldSapphireNecklace_Arcane02 | global_shop::purchase_reward, jewellery rolled per purchase | 438 distinct item instances |
| SigilShop_Armor_GlassHelmet_Temp06_FortMgc | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineEbonyMace | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_EffectIngredients_ImpStool | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Armor_OrcishPlateBoots_Arcane01 | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_EbonyHandAxe_Temp06_MgcDmg | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSteelHandAxe | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| DecoOffer_M_Abyss3_2019-02-06 00:00_2019-02-09 23:59 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_DivineLeatherGauntlets | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Armor_ElvenArmor_Arcane02 | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| IAP_ElementalPack_04 | global_shop::purchase_reward, captured retail grant (fixed) |  |
| SigilShop_Jewelry_EbonyFaeriteNecklace_Armsman_FortMgcRegen | global_shop::purchase_reward, jewellery rolled per purchase | 496 distinct item instances |
| SigilShop_DivineIronGreatsword | global_shop::purchase_reward, APK offer contents | fixed by its APK definition; 1 distinct item instances |
| SigilShop_DivineSilverDagger | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |
| SigilShop_Weapon_DaedricDagger_Temp08_Combo | global_shop::purchase_reward, captured retail grant (fixed) | fixed by its APK definition; 1 distinct item instances |

</details>

### Allowlist (known asserted failures)

| Subject | Check | Finding | Reason | Firing |
|---|---|---|---|---:|
| `chest:` | Validity | "but no GRADING properties" | Silver/Gold treasury (and daily) chest rings and necklaces carry a grade but no GRADING: `script/extract_chest_loot.py` keeps only ENCHANTING when an item has any, so 77 of the 100 retail chest jewels (every enchanted one) lost their bonuses. Fix: fix/audit-chest-jewelry-grading (#525). | **0: remove the entry** |
| `chest:` | Variety | "frozen item" | Treasury tiers 1-3 copy one retail piece per item slot from the nearest openings; where few of them paid an item (Wooden: 44 of 154) one piece is every item. Fix: fix/audit-treasury-gear-generated (#527; generated around the retail piece, as Legendary chests). | **0: remove the entry** |
| `chest:` | Variety | "repeating jewellery" | Same cause as the frozen items: one retail ring/necklace instance per slot, verbatim. Fix: fix/audit-treasury-gear-generated. | **0: remove the entry** |
| `merchant:` | Variety | "frozen catalog" | Building levels with ONE retail catalog (Forge 5-6, Workshop 2, Alchemist 1 and 6) were measured as exactly that catalog, so every window lists it again. One catalog says what the level CAN sell, not how often; widening it needs a model of the neighbouring levels (follow-up to #397, which fixed the level-9 Alchemist's rare potions). | 5 |

<!-- END GENERATED -->
