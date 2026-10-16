# Level curve, levels 1-48 (report #398)

HauDrauf (#398, 2026-10-16, translated): "It should rise with the player level,
like the offers in the shops — that's how the original was. Later you no longer
got Histcarp, Pearls etc., only things you can actually use. We'll test the exact
gradations."

The retail captures are too thin to drive this per source, so it is a rule. He
approved the narrowing it brings: low-tier items stop appearing at higher levels.
From level 49 nothing here applies; every source already pays as level 100
(fork #534, with #535's event soul-gem cap at 49-60).

Code: `blades_lib/src/features/level_curve.rs`. Every tunable is a named constant
there, each citing #398.

## The rule

**Gear** (weapons, armour, shields; never rings, necklaces or artifacts). At level
`L` only material tiers whose APK required level `R` satisfies
`L - GEAR_WINDOW_BELOW <= R <= L + GEAR_WINDOW_ABOVE` are paid. Each is weighted
by a triangle peaking at `R = L`: 1 at `R = L`, falling linearly to 0 one level
past either edge of the window. A piece the source drew is moved to the drawn
tier as the same kind of equipment (a sword stays a weapon, a helmet a helmet),
keeping its tempering, enchantment count and enchant tier. An empty window would
fall back to the lowest tier; with today's tiers (1, 8, 13, 18, 23, 28, 33, 39,
45) it never is.

**Crafting materials** (ingredients, ingots and ores, gems, town materials). Each
has an introduction level `I`: the unlock level of its APK tier (tier 3 = 8, tier 8
= 33, tier 10 = 45). A material can drop from `I - MATERIAL_LOOKAHEAD`; its weight
is full up to `I`, then fades linearly to 0 at `I + MATERIAL_FADE_LEVELS`. A
stack that fades is replaced by one the same source pays at that level, among the
materials still at `MATERIAL_REPLACEMENT_MIN_WEIGHT` or more, at the quantity the
source pays it in. So a fading material keeps its own share times its weight, and
the share it loses goes to the newer tiers. Soul gems, the seven enchanting
ingredients, gold and every quantity keep their existing per-level rules, and so
does the 8/17/75 jewellery split.

### The constants

| Constant | Default | Meaning |
|---|---|---|
| `GEAR_WINDOW_BELOW` | 10 | lowest gear tier paid: unlocked at `L - 10` or later |
| `GEAR_WINDOW_ABOVE` | 2 | highest gear tier paid: unlocked by `L + 2` |
| `MATERIAL_LOOKAHEAD` | 2 | a material may drop from `I - 2` |
| `MATERIAL_FADE_LEVELS` | 20 | a material's weight reaches 0 at `I + 20` |
| `MATERIAL_REPLACEMENT_MIN_WEIGHT` | 0.5 | what a faded stack can turn into: materials still at half weight or more |
| `MATERIAL_POOL_WINDOW` | 2 | dungeon loot, event and story-quest rewards replace from retail's dungeon materials within 2 levels |
| `CHEST_POOL_MIN_KINDS` | 4 | a chest whose own tier has fewer replacement kinds at its level also draws from the dungeon materials |
| `FADE_TOWN_MATERIALS` | `true` | the town materials fade too (all tier 1, so gone from 21). `false` keeps them at retail's rates |
| `MATERIAL_TIER_INTRODUCTION_LEVEL` | 1, 1, 8, 13, 18, 23, 28, 33, 39, 45, 54, 75, 77 | `I` per APK tier 1-13 |

### Why the APK tier and not the loot tables' first listing

The lowest level any loot table lists a material at is polluted by low-level
enemies in high-level dungeons and single stray rows: Ebony Ingot is listed by a
level-8 enemy, Dragon Bone by a level-7 one, Daedra Heart at 4. Taken literally,
Ebony would fade out at 28, before Ebony gear unlocks at 33. The shop stock is
keyed by building level, not character level (the Forge sells Dwarven ingots at
its first tier), so it cannot date a material either. The APK gives every
material a tier on the gear ladder, and retail's own event rewards step their
materials at exactly those levels (#362, 409 observations, none contradicting).
`blades_lib/src/material_tiers.json` keeps each source's first listing beside the
tier for reference (`script/extract_material_tiers.py`).

## Where it applies

Every generator whose level input is 1-48:

- treasury chests of every tier (Wooden, Silver, Gold, Elder, Legendary, the
  tutorial chest), at the chest's level;
- dungeon enemy and container loot (story quests, events, jobs), at the level each
  draw is read at;
- event milestone and final rewards go through it too, but change nothing in
  today's data: retail already levels them (the band, #333, and the material
  ladders, #362), and the only other materials they pay are Lumber and Limestone,
  which retail paid in event rewards at every level up to 100. Fading those would
  make a level-48 event pay more than the level-100 reward every 49+ character
  gets (#534), so they stay;
- story-quest completion rewards;
- the Abyss bonus material (#383). Retail's own Abyss corpse loot, the Abyss rungs
  and `/end` are Abyss tables, not these, and are untouched.

Not touched: daily rewards, level-up rewards, store purchases (the Golden, Epic
and Legendary store chests), Sigil-shop and merchant stock. A gifted chest is a
treasury chest and follows the curve.

## The numbers

Regenerate with
`LEVEL_CURVE_WRITE=1 cargo test -p server level_curve_table -- --ignored`.

<!-- level-curve-table:start -->

### Crafting materials: introduction level and weight by level

`I` is the material's APK tier's unlock level. The weight is the factor on what a source pays it at: full from `I - 2` to `I`, then down to 0 at `I + 20`. Town materials are tier 1. Soul gems and the seven enchanting ingredients are never faded.

| APK tier | I | Materials | L5 | L10 | L15 | L20 | L25 | L30 | L35 | L40 | L45 | L48 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 1 | Brass Ingot, Bronze, Clay, Crystal, Dye, Fabric, Garlic, Iron Ingot, Limestone, Lumber, Marble, Pearl, Pelt, Seeds | 0.80 | 0.55 | 0.30 | 0.05 | - | - | - | - | - | - |
| 2 | 1 | Lavender, Leather, Steel Ingot, Topaz | 0.80 | 0.55 | 0.30 | 0.05 | - | - | - | - | - | - |
| 3 | 8 | Garnet, Histcarp, Scrib Chitin, Silver Ingot | - | 0.90 | 0.65 | 0.40 | 0.15 | - | - | - | - | - |
| 4 | 13 | Amethyst, Blue Dartwing, Orichalcum Ingot | - | - | 0.90 | 0.65 | 0.40 | 0.15 | - | - | - | - |
| 5 | 18 | Dwarven Metal Ingot, Ectoplasm, Gold Ingot, Ruby | - | - | - | 0.90 | 0.65 | 0.40 | 0.15 | - | - | - |
| 6 | 23 | Moon Sugar, Moonstone Ingot, Quicksilver Ingot, Sapphire | - | - | - | - | 0.90 | 0.65 | 0.40 | 0.15 | - | - |
| 7 | 28 | Chaurus Chitin, Emerald, Malachite Ingot, Nightshade | - | - | - | - | - | 0.90 | 0.65 | 0.40 | 0.15 | - |
| 8 | 33 | Diamond, Ebony Ingot, Giant's Toe, Stalhrim | - | - | - | - | - | - | 0.90 | 0.65 | 0.40 | 0.25 |
| 9 | 39 | Atronite, Daedra Heart, Daedroth Tooth | - | - | - | - | - | - | - | 0.95 | 0.70 | 0.55 |
| 10 | 45 | Dragon Bones, Dragon Claw, Dragon Scales, Faerite | - | - | - | - | - | - | - | - | 1.00 | 0.85 |
| 11 | 54 | Pyrine | - | - | - | - | - | - | - | - | - | - |
| 12 | 75 | Arentine | - | - | - | - | - | - | - | - | - | - |
| 13 | 77 | Firebreath Opal | - | - | - | - | - | - | - | - | - | - |

### Gear: material tier shares (weapons, armour, shields)

The rule (`gear_tier_shares`), and the shares measured over 1,500 chests of every tier (300 each, Wooden to Legendary) and the dungeon corpses at that level. Rings, necklaces and artifacts are not moved.

| Level | Rule | Measured |
|---|---|---|
| 5 | Iron/Steel 100% | Iron/Steel 100% |
| 10 | Iron/Steel 18%, Silver 82% | Iron/Steel 19%, Silver 81% |
| 15 | Silver 31%, Orcish 69% | Silver 30%, Orcish 70% |
| 20 | Orcish 31%, Dwarven 69% | Orcish 31%, Dwarven 69% |
| 25 | Dwarven 31%, Elven 69% | Dwarven 29%, Elven 71% |
| 30 | Elven 31%, Glass 69% | Elven 30%, Glass 70% |
| 35 | Glass 31%, Ebony 69% | Glass 30%, Ebony 70% |
| 40 | Ebony 29%, Daedric 71% | Ebony 28%, Daedric 72% |
| 45 | Daedric 31%, Dragon 69% | Daedric 32%, Dragon 68% |
| 48 | Daedric 20%, Dragon 80% | Daedric 20%, Dragon 80% |

### Crafting materials: top shares by source

Share of the stacks of curve materials (soul gems and the seven enchanting ingredients are left out: they are not faded and their shares do not move). Before = retail's tables at that level; after = the curve.

| Level | Source | Before | After |
|---|---|---|---|
| 5 | corpses | Limestone 26%, Lumber 21%, Garlic 12%, Pelt 10%, Clay 7%, (+9 more) | Limestone 23%, Lumber 20%, Pelt 14%, Garlic 11%, Pearl 6%, (+13 more) |
| 5 | containers | Limestone 33%, Lumber 31%, Iron Ingot 9%, Pearl 7%, Pelt 6%, (+10 more) | Lumber 37%, Limestone 26%, Iron Ingot 9%, Pearl 5%, Pelt 4%, (+12 more) |
| 10 | corpses | Limestone 28%, Garlic 18%, Lumber 17%, Bronze 7%, Pelt 6%, (+8 more) | Limestone 26%, Lumber 17%, Garlic 12%, Pelt 8%, Iron Ingot 6%, (+17 more) |
| 10 | containers | Limestone 35%, Lumber 32%, Pelt 5%, Iron Ingot 5%, Pearl 4%, (+12 more) | Limestone 29%, Lumber 28%, Pelt 6%, Iron Ingot 5%, Pearl 5%, (+16 more) |
| 15 | corpses | Limestone 26%, Lumber 20%, Clay 13%, Garlic 7%, Pearl 6%, (+14 more) | Orichalcum Ingot 20%, Silver Ingot 16%, Garnet 11%, Scrib Chitin 9%, Blue Dartwing 7%, (+17 more) |
| 15 | containers | Lumber 34%, Limestone 27%, Pelt 5%, Topaz 4%, Iron Ingot 4%, (+14 more) | Orichalcum Ingot 19%, Silver Ingot 15%, Lumber 12%, Scrib Chitin 12%, Garnet 11%, (+15 more) |
| 20 | corpses | Limestone 25%, Lumber 21%, Clay 7%, Garlic 7%, Marble 5%, (+23 more) | Dwarven Metal Ingot 30%, Orichalcum Ingot 28%, Amethyst 17%, Blue Dartwing 9%, Ruby 6%, (+17 more) |
| 20 | containers | Lumber 35%, Limestone 30%, Orichalcum Ingot 5%, Steel Ingot 4%, Dwarven Metal Ingot 4%, (+17 more) | Dwarven Metal Ingot 30%, Orichalcum Ingot 27%, Amethyst 17%, Blue Dartwing 7%, Ruby 6%, (+11 more) |
| 25 | corpses | Lumber 24%, Limestone 22%, Lavender 9%, Histcarp 8%, Leather 6%, (+22 more) | Dwarven Metal Ingot 34%, Moonstone Ingot 19%, Ruby 10%, Quicksilver Ingot 9%, Sapphire 7%, (+8 more) |
| 25 | containers | Lumber 35%, Limestone 33%, Dwarven Metal Ingot 6%, Amethyst 5%, Silver Ingot 4%, (+14 more) | Dwarven Metal Ingot 37%, Moonstone Ingot 18%, Ruby 9%, Sapphire 9%, Gold Ingot 7%, (+8 more) |
| 30 | corpses | Limestone 22%, Lumber 21%, Clay 9%, Blue Dartwing 6%, Orichalcum Ingot 5%, (+25 more) | Sapphire 19%, Moonstone Ingot 17%, Quicksilver Ingot 16%, Chaurus Chitin 15%, Moon Sugar 10%, (+9 more) |
| 30 | containers | Lumber 33%, Limestone 29%, Orichalcum Ingot 8%, Dwarven Metal Ingot 4%, Sapphire 4%, (+17 more) | Sapphire 21%, Moonstone Ingot 17%, Chaurus Chitin 16%, Quicksilver Ingot 14%, Moon Sugar 10%, (+8 more) |
| 35 | corpses | Limestone 27%, Lumber 23%, Clay 9%, Orichalcum Ingot 6%, Bronze 5%, (+25 more) | Malachite Ingot 24%, Emerald 20%, Nightshade 13%, Chaurus Chitin 13%, Ebony Ingot 9%, (+9 more) |
| 35 | containers | Limestone 32%, Lumber 30%, Quicksilver Ingot 5%, Dwarven Metal Ingot 5%, Malachite Ingot 4%, (+15 more) | Malachite Ingot 26%, Emerald 19%, Nightshade 14%, Chaurus Chitin 14%, Stalhrim 9%, (+8 more) |
| 40 | corpses | Limestone 23%, Lumber 17%, Clay 9%, Ectoplasm 9%, Bronze 5%, (+23 more) | Diamond 30%, Ebony Ingot 23%, Daedra Heart 23%, Stalhrim 11%, Atronite 10%, (+6 more) |
| 40 | containers | Lumber 36%, Limestone 27%, Emerald 6%, Dwarven Metal Ingot 4%, Diamond 3%, (+16 more) | Diamond 33%, Daedra Heart 22%, Ebony Ingot 18%, Stalhrim 14%, Atronite 9%, (+6 more) |
| 45 | corpses | Lumber 29%, Limestone 14%, Moon Sugar 14%, Clay 12%, Bronze 5%, (+13 more) | Daedra Heart 31%, Atronite 22%, Dragon Scales 20%, Dragon Bones 19%, Faerite 7%, (+1 more) |
| 45 | containers | Limestone 33%, Lumber 28%, Quicksilver Ingot 5%, Moonstone Ingot 5%, Diamond 4%, (+14 more) | Daedra Heart 35%, Atronite 23%, Dragon Scales 19%, Dragon Bones 16%, Faerite 5%, (+6 more) |
| 48 | corpses | Lumber 21%, Limestone 19%, Clay 13%, Bronze 7%, Moon Sugar 6%, (+16 more) | Daedra Heart 30%, Dragon Bones 23%, Atronite 22%, Dragon Scales 19%, Faerite 7%, (+2 more) |
| 48 | containers | Limestone 42%, Lumber 23%, Atronite 6%, Dragon Bones 6%, Quicksilver Ingot 6%, (+5 more) | Daedra Heart 33%, Dragon Bones 22%, Dragon Scales 20%, Atronite 19%, Faerite 6%, (+2 more) |

| Level | Chest | Before | After |
|---|---|---|---|
| 5 | Wooden | Lumber 24%, Pelt 19%, Pearl 16%, Limestone 10%, Iron Ingot 9%, (+4 more) | Lumber 23%, Pelt 19%, Pearl 15%, Iron Ingot 11%, Limestone 10%, (+4 more) |
| 5 | Silver | Clay 39%, Garlic 13%, Lumber 13%, Limestone 12%, Iron Ingot 5%, (+6 more) | Clay 35%, Garlic 13%, Lumber 13%, Limestone 12%, Brass Ingot 5%, (+6 more) |
| 5 | Gold | Clay 35%, Lumber 12%, Bronze 8%, Orichalcum Ingot 6%, Fabric 6%, (+10 more) | Clay 32%, Lumber 14%, Bronze 10%, Steel Ingot 8%, Fabric 8%, (+6 more) |
| 5 | Elder | Topaz 16%, Clay 16%, Garnet 13%, Lumber 8%, Limestone 8%, (+15 more) | Clay 15%, Topaz 14%, Lumber 11%, Steel Ingot 9%, Bronze 8%, (+10 more) |
| 5 | Legendary | Topaz 14%, Clay 14%, Garnet 14%, Amethyst 12%, Leather 10%, (+12 more) | Clay 14%, Topaz 13%, Lumber 12%, Steel Ingot 10%, Bronze 9%, (+10 more) |
| 10 | Wooden | Moonstone Ingot 18%, Pearl 18%, Lumber 17%, Bronze 9%, Quicksilver Ingot 6%, (+7 more) | Pearl 24%, Lumber 23%, Bronze 12%, Pelt 7%, Seeds 6%, (+5 more) |
| 10 | Silver | Clay 38%, Bronze 8%, Limestone 7%, Lumber 6%, Steel Ingot 5%, (+14 more) | Clay 30%, Bronze 9%, Limestone 9%, Steel Ingot 7%, Lumber 6%, (+9 more) |
| 10 | Gold | Clay 35%, Lumber 11%, Orichalcum Ingot 10%, Bronze 7%, Scrib Chitin 5%, (+10 more) | Clay 28%, Lumber 13%, Scrib Chitin 10%, Bronze 10%, Topaz 6%, (+8 more) |
| 10 | Elder | Clay 15%, Garnet 14%, Amethyst 10%, Topaz 7%, Limestone 6%, (+17 more) | Clay 14%, Garnet 13%, Scrib Chitin 10%, Lumber 9%, Bronze 8%, (+12 more) |
| 10 | Legendary | Amethyst 13%, Clay 13%, Garnet 13%, Ruby 12%, Scrib Chitin 7%, (+15 more) | Clay 13%, Garnet 12%, Scrib Chitin 11%, Lumber 9%, Bronze 9%, (+12 more) |
| 15 | Wooden | Lumber 28%, Limestone 24%, Moonstone Ingot 17%, Bronze 7%, Marble 7%, (+3 more) | Silver Ingot 43%, Orichalcum Ingot 10%, Scrib Chitin 9%, Lumber 8%, Limestone 7%, (+7 more) |
| 15 | Silver | Clay 43%, Histcarp 8%, Lumber 6%, Nightshade 6%, Fabric 6%, (+11 more) | Histcarp 26%, Orichalcum Ingot 20%, Scrib Chitin 16%, Clay 11%, Silver Ingot 7%, (+10 more) |
| 15 | Gold | Clay 34%, Lumber 12%, Orichalcum Ingot 8%, Marble 8%, Garnet 7%, (+10 more) | Orichalcum Ingot 22%, Garnet 17%, Clay 11%, Amethyst 11%, Blue Dartwing 10%, (+9 more) |
| 15 | Elder | Clay 17%, Amethyst 16%, Ruby 11%, Lumber 8%, Sapphire 8%, (+17 more) | Amethyst 17%, Orichalcum Ingot 16%, Garnet 14%, Blue Dartwing 11%, Silver Ingot 10%, (+11 more) |
| 15 | Legendary | Amethyst 15%, Clay 15%, Ruby 15%, Sapphire 14%, Orichalcum Ingot 10%, (+13 more) | Amethyst 16%, Orichalcum Ingot 15%, Garnet 15%, Blue Dartwing 13%, Histcarp 10%, (+11 more) |
| 20 | Wooden | Lumber 14%, Quicksilver Ingot 13%, Moonstone Ingot 11%, Pearl 9%, Limestone 9%, (+7 more) | Ruby 38%, Ectoplasm 25%, Orichalcum Ingot 15%, Dwarven Metal Ingot 12%, Amethyst 5%, (+5 more) |
| 20 | Silver | Clay 37%, Limestone 12%, Lumber 11%, Malachite Ingot 8%, Orichalcum Ingot 6%, (+10 more) | Orichalcum Ingot 31%, Blue Dartwing 25%, Dwarven Metal Ingot 22%, Amethyst 18%, Clay 3%, (+5 more) |
| 20 | Gold | Clay 28%, Marble 12%, Lumber 10%, Histcarp 8%, Orichalcum Ingot 6%, (+11 more) | Orichalcum Ingot 26%, Amethyst 19%, Ruby 19%, Dwarven Metal Ingot 16%, Blue Dartwing 6%, (+12 more) |
| 20 | Elder | Clay 16%, Sapphire 15%, Ruby 13%, Emerald 9%, Dwarven Metal Ingot 7%, (+16 more) | Ruby 19%, Orichalcum Ingot 19%, Amethyst 19%, Dwarven Metal Ingot 18%, Blue Dartwing 11%, (+11 more) |
| 20 | Legendary | Sapphire 15%, Clay 15%, Ruby 15%, Emerald 13%, Dwarven Metal Ingot 9%, (+17 more) | Orichalcum Ingot 19%, Amethyst 19%, Ruby 19%, Dwarven Metal Ingot 18%, Blue Dartwing 11%, (+9 more) |
| 25 | Wooden | Limestone 26%, Clay 11%, Quicksilver Ingot 10%, Bronze 10%, Ectoplasm 9%, (+6 more) | Moonstone Ingot 27%, Quicksilver Ingot 27%, Ectoplasm 20%, Sapphire 16%, Ruby 10% |
| 25 | Silver | Clay 39%, Moon Sugar 10%, Malachite Ingot 10%, Lumber 7%, Limestone 7%, (+8 more) | Moon Sugar 35%, Dwarven Metal Ingot 24%, Quicksilver Ingot 22%, Moonstone Ingot 7%, Ruby 4%, (+4 more) |
| 25 | Gold | Clay 37%, Ectoplasm 13%, Lumber 9%, Marble 7%, Malachite Ingot 6%, (+6 more) | Ectoplasm 31%, Ruby 24%, Sapphire 20%, Dwarven Metal Ingot 13%, Moonstone Ingot 6%, (+3 more) |
| 25 | Elder | Clay 16%, Sapphire 15%, Ruby 13%, Emerald 9%, Dwarven Metal Ingot 7%, (+16 more) | Ectoplasm 19%, Sapphire 18%, Ruby 18%, Dwarven Metal Ingot 15%, Moonstone Ingot 9%, (+5 more) |
| 25 | Legendary | Sapphire 14%, Clay 14%, Emerald 14%, Diamond 12%, Dwarven Metal Ingot 7%, (+13 more) | Sapphire 18%, Ectoplasm 18%, Ruby 17%, Dwarven Metal Ingot 15%, Moonstone Ingot 13%, (+3 more) |
| 30 | Wooden | Limestone 22%, Lumber 15%, Ruby 15%, Moonstone Ingot 9%, Crystal 8%, (+6 more) | Moonstone Ingot 27%, Chaurus Chitin 19%, Malachite Ingot 18%, Sapphire 16%, Quicksilver Ingot 15%, (+1 more) |
| 30 | Silver | Clay 37%, Limestone 11%, Malachite Ingot 9%, Lumber 9%, Orichalcum Ingot 6%, (+10 more) | Malachite Ingot 33%, Chaurus Chitin 27%, Nightshade 15%, Sapphire 6%, Moonstone Ingot 6%, (+6 more) |
| 30 | Gold | Clay 34%, Sapphire 10%, Marble 7%, Limestone 7%, Malachite Ingot 6%, (+11 more) | Sapphire 25%, Emerald 21%, Malachite Ingot 21%, Chaurus Chitin 20%, Moonstone Ingot 12%, (+2 more) |
| 30 | Elder | Clay 17%, Emerald 15%, Sapphire 11%, Diamond 9%, Limestone 7%, (+16 more) | Emerald 19%, Sapphire 19%, Chaurus Chitin 19%, Malachite Ingot 18%, Moonstone Ingot 15%, (+6 more) |
| 30 | Legendary | Diamond 15%, Clay 15%, Emerald 15%, Atronite 12%, Limestone 6%, (+16 more) | Emerald 19%, Malachite Ingot 19%, Sapphire 19%, Chaurus Chitin 19%, Moonstone Ingot 16%, (+3 more) |
| 35 | Wooden | Quicksilver Ingot 13%, Limestone 13%, Clay 13%, Bronze 11%, Ectoplasm 10%, (+6 more) | Malachite Ingot 38%, Emerald 36%, Nightshade 5%, Quicksilver Ingot 5%, Chaurus Chitin 4%, (+7 more) |
| 35 | Silver | Clay 38%, Lumber 10%, Limestone 9%, Stalhrim 8%, Sapphire 6%, (+9 more) | Stalhrim 35%, Ebony Ingot 23%, Malachite Ingot 19%, Chaurus Chitin 18%, Sapphire 3%, (+2 more) |
| 35 | Gold | Clay 36%, Limestone 13%, Sapphire 9%, Seeds 5%, Emerald 5%, (+13 more) | Emerald 28%, Malachite Ingot 27%, Nightshade 23%, Chaurus Chitin 15%, Sapphire 3%, (+5 more) |
| 35 | Elder | Clay 18%, Diamond 15%, Atronite 10%, Emerald 8%, Limestone 7%, (+22 more) | Emerald 20%, Nightshade 20%, Malachite Ingot 20%, Diamond 17%, Chaurus Chitin 16%, (+6 more) |
| 35 | Legendary | Diamond 16%, Clay 16%, Atronite 16%, Faerite 11%, Chaurus Chitin 7%, (+13 more) | Malachite Ingot 20%, Emerald 20%, Nightshade 19%, Diamond 18%, Chaurus Chitin 18%, (+3 more) |
| 40 | Wooden | Lumber 25%, Clay 16%, Quicksilver Ingot 16%, Ectoplasm 12%, Limestone 8%, (+4 more) | Diamond 32%, Daedra Heart 24%, Ebony Ingot 20%, Stalhrim 12%, Atronite 9%, (+3 more) |
| 40 | Silver | Clay 40%, Lumber 14%, Moon Sugar 12%, Limestone 10%, Chaurus Chitin 8%, (+6 more) | Ebony Ingot 29%, Giant's Toe 24%, Diamond 15%, Daedra Heart 13%, Stalhrim 6%, (+6 more) |
| 40 | Gold | Clay 33%, Limestone 8%, Diamond 8%, Marble 8%, Lumber 7%, (+13 more) | Diamond 31%, Ebony Ingot 28%, Atronite 19%, Giant's Toe 14%, Nightshade 2%, (+4 more) |
| 40 | Elder | Clay 17%, Diamond 15%, Atronite 11%, Emerald 7%, Limestone 7%, (+22 more) | Diamond 23%, Ebony Ingot 23%, Atronite 22%, Giant's Toe 19%, Emerald 4%, (+7 more) |
| 40 | Legendary | Diamond 16%, Clay 16%, Atronite 16%, Faerite 11%, Ebony Ingot 8%, (+8 more) | Diamond 24%, Ebony Ingot 24%, Atronite 24%, Giant's Toe 21%, Stalhrim 5%, (+1 more) |
| 45 | Wooden | Lumber 23%, Clay 15%, Limestone 12%, Seeds 10%, Ectoplasm 9%, (+8 more) | Daedra Heart 38%, Atronite 21%, Dragon Scales 17%, Dragon Bones 16%, Faerite 4%, (+3 more) |
| 45 | Silver | Clay 40%, Moon Sugar 14%, Lumber 13%, Limestone 9%, Chaurus Chitin 7%, (+6 more) | Daedra Heart 29%, Atronite 22%, Dragon Bones 20%, Dragon Scales 19%, Faerite 6%, (+5 more) |
| 45 | Gold | Clay 33%, Limestone 10%, Marble 10%, Emerald 9%, Ebony Ingot 9%, (+9 more) | Atronite 29%, Daedra Heart 29%, Dragon Bones 14%, Dragon Scales 12%, Ebony Ingot 4%, (+5 more) |
| 45 | Elder | Clay 16%, Atronite 16%, Faerite 14%, Pyrine 9%, Daedra Heart 8%, (+11 more) | Atronite 21%, Daedra Heart 21%, Faerite 20%, Dragon Scales 18%, Dragon Bones 18%, (+1 more) |
| 45 | Legendary | Clay 16%, Atronite 16%, Faerite 15%, Pyrine 12%, Daedra Heart 7%, (+14 more) | Atronite 21%, Daedra Heart 20%, Faerite 20%, Dragon Scales 17%, Dragon Bones 17%, (+2 more) |
| 48 | Wooden | Lumber 27%, Clay 17%, Limestone 12%, Seeds 7%, Bronze 6%, (+6 more) | Daedra Heart 30%, Atronite 23%, Dragon Bones 19%, Dragon Scales 18%, Faerite 6%, (+2 more) |
| 48 | Silver | Clay 40%, Moon Sugar 14%, Lumber 13%, Limestone 10%, Chaurus Chitin 7%, (+6 more) | Daedra Heart 29%, Dragon Bones 23%, Atronite 23%, Dragon Scales 18%, Faerite 6%, (+2 more) |
| 48 | Gold | Clay 33%, Emerald 11%, Marble 9%, Ebony Ingot 7%, Moon Sugar 6%, (+8 more) | Atronite 30%, Dragon Scales 27%, Daedra Heart 25%, Dragon Bones 11%, Faerite 3%, (+3 more) |
| 48 | Elder | Clay 16%, Atronite 16%, Faerite 14%, Pyrine 9%, Daedra Heart 7%, (+11 more) | Atronite 21%, Dragon Scales 21%, Daedra Heart 21%, Faerite 18%, Dragon Bones 17%, (+1 more) |
| 48 | Legendary | Clay 16%, Atronite 16%, Faerite 15%, Pyrine 11%, Daedra Heart 7%, (+14 more) | Dragon Scales 21%, Atronite 20%, Daedra Heart 20%, Faerite 18%, Dragon Bones 16%, (+2 more) |

| Level | Source | Before | After |
|---|---|---|---|
| 5 | story-quest rewards | Lumber 12%, Clay 10%, Garlic 10%, Limestone 10%, Marble 6%, (+22 more) | Limestone 21%, Lumber 20%, Garlic 10%, Clay 8%, Marble 6%, (+13 more) |
| 5 | Abyss bonus material | | Limestone 21%, Lumber 20%, Garlic 15%, Pelt 11%, Iron Ingot 10%, (+12 more) |
| 10 | story-quest rewards | Lumber 12%, Clay 10%, Garlic 10%, Limestone 10%, Marble 6%, (+22 more) | Limestone 21%, Lumber 19%, Garlic 9%, Clay 7%, Marble 6%, (+16 more) |
| 10 | Abyss bonus material | | Limestone 24%, Lumber 21%, Garlic 12%, Pelt 8%, Iron Ingot 8%, (+17 more) |
| 15 | story-quest rewards | Lumber 12%, Clay 10%, Garlic 10%, Limestone 10%, Marble 6%, (+22 more) | Orichalcum Ingot 18%, Silver Ingot 17%, Scrib Chitin 12%, Garnet 10%, Blue Dartwing 8%, (+17 more) |
| 15 | Abyss bonus material | | Orichalcum Ingot 20%, Silver Ingot 19%, Garnet 11%, Scrib Chitin 8%, Blue Dartwing 7%, (+18 more) |
| 20 | story-quest rewards | Lumber 12%, Clay 10%, Garlic 10%, Limestone 10%, Marble 6%, (+22 more) | Dwarven Metal Ingot 29%, Orichalcum Ingot 22%, Amethyst 18%, Gold Ingot 9%, Blue Dartwing 8%, (+13 more) |
| 20 | Abyss bonus material | | Dwarven Metal Ingot 33%, Orichalcum Ingot 28%, Amethyst 15%, Blue Dartwing 6%, Ruby 5%, (+17 more) |
| 25 | story-quest rewards | Lumber 12%, Clay 10%, Garlic 10%, Limestone 10%, Marble 6%, (+22 more) | Dwarven Metal Ingot 32%, Moonstone Ingot 18%, Sapphire 10%, Ruby 9%, Quicksilver Ingot 8%, (+6 more) |
| 25 | Abyss bonus material | | Dwarven Metal Ingot 41%, Moonstone Ingot 17%, Ruby 9%, Quicksilver Ingot 8%, Sapphire 6%, (+9 more) |
| 30 | story-quest rewards | Lumber 12%, Clay 10%, Garlic 10%, Limestone 10%, Marble 6%, (+22 more) | Sapphire 21%, Chaurus Chitin 17%, Quicksilver Ingot 17%, Moonstone Ingot 16%, Moon Sugar 9%, (+7 more) |
| 30 | Abyss bonus material | | Sapphire 19%, Moonstone Ingot 17%, Chaurus Chitin 16%, Quicksilver Ingot 15%, Moon Sugar 10%, (+8 more) |
| 35 | story-quest rewards | Lumber 12%, Clay 10%, Garlic 10%, Limestone 10%, Marble 6%, (+22 more) | Malachite Ingot 22%, Emerald 19%, Chaurus Chitin 15%, Nightshade 12%, Ebony Ingot 10%, (+9 more) |
| 35 | Abyss bonus material | | Malachite Ingot 27%, Nightshade 15%, Emerald 15%, Chaurus Chitin 13%, Ebony Ingot 8%, (+10 more) |
| 40 | story-quest rewards | Lumber 12%, Clay 10%, Garlic 10%, Limestone 10%, Marble 6%, (+22 more) | Diamond 33%, Daedra Heart 23%, Ebony Ingot 21%, Stalhrim 13%, Atronite 8%, (+3 more) |
| 40 | Abyss bonus material | | Diamond 33%, Daedra Heart 23%, Ebony Ingot 20%, Stalhrim 11%, Atronite 8%, (+7 more) |
| 45 | story-quest rewards | Lumber 12%, Clay 10%, Garlic 10%, Limestone 10%, Marble 6%, (+22 more) | Daedra Heart 35%, Dragon Scales 21%, Atronite 20%, Dragon Bones 17%, Faerite 5%, (+2 more) |
| 45 | Abyss bonus material | | Daedra Heart 34%, Atronite 23%, Dragon Bones 19%, Dragon Scales 15%, Faerite 3%, (+7 more) |
| 48 | story-quest rewards | Lumber 12%, Clay 10%, Garlic 10%, Limestone 10%, Marble 6%, (+22 more) | Daedra Heart 32%, Dragon Bones 20%, Atronite 20%, Dragon Scales 20%, Faerite 6%, (+2 more) |
| 48 | Abyss bonus material | | Daedra Heart 35%, Dragon Bones 23%, Atronite 19%, Dragon Scales 16%, Faerite 4%, (+3 more) |


<!-- level-curve-table:end -->
