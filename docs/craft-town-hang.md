# The craft/town loading hang

**Status: contained, not fixed.** `get_crafts` detaches craft jobs from real
buildings so nobody can be bricked. The underlying client behaviour is still
not understood, and the containment should be deleted the day it is.

## Symptom

The client completes its entire API boot — sync, characters, wallets,
inventory, towns, quests, shops, every call a 200 in a few milliseconds — then
sits on the title screen forever. No error, anywhere: not in logcat, not in the
server log, not in the transcript. The process keeps rendering at ~60% of a
core, so it does not look hung from outside.

Players report it as "can't connect", which is what makes it expensive: it
reads as a VPN or account problem and gets triaged as one.

## What actually triggers it

A craft job whose `buildingId` **resolves to a building in that character's own
town**.

Measured on a clean rig (emulator running the byte-identical distributed APK,
full asset cache, a copy of a real character so nothing live was touched):

| the one thing changed | result |
|---|---|
| `buildingId` → a real building in the town | **stalls** |
| `buildingId` → any unresolvable uuid | **loads** |

Everything else was held constant: same job id, same recipe, same crafting
type, same results, same completion time, same town, same character.

## What it is NOT

Each of these was tested and ruled out, so nobody re-treads them:

* **Not completion.** An in-progress craft (completing in 24h) stalls too.
* **Not the crafting type.** `c9d3b3aa` is `Alchemy`, and
  `building_upgrades.json` lists it in that AlchemistShop's
  `allowedCraftingTypes`. Another character loads fine carrying a job with the
  *same* crafting type — his simply points at a building he does not have.
* **Not the recipe.** 13 of 14 craft jobs on prod have a `recipeId` that is not
  in `recipes.json` (they live in `recipe_crafting_types.json`), including on
  characters that load perfectly.
* **Not `stored_crafting_type_is_unserveable`.** The existing repair path from
  report #34 does not fire here: `craftingTypeId != recipeId` and it is not nil.
* **Not the building's `state`.** Setting it to something else changes nothing
  (and only `NORMAL`, `UPGRADING`, `BUILDING` occur in captures anyway).
* **Not the town.** The same town loads fine once the craft job is detached.
* **Not assets, the character, the inventory, the wallet or server_state.** All
  bisected out on a copy, column by column.

## Blast radius

Every player with a town who crafts. It does not bite immediately — the craft
succeeds and the session continues — it bites on the **next load**. Three
characters were bricked this way; one crafted once on 2026-08-26 and never got
back in. The repair logic that was supposed to prevent this shipped 2026-08-19,
six days earlier, so its absence is not the explanation.

## The containment

`get_crafts` reads the character's town, and for any job whose `buildingId` is
one of that town's buildings, emits a **stand-in id** instead
(`detached_building_id`: the real id with high bits flipped, so it is stable
across reads and cannot collide with a real building).

Deliberately a detach and not a delete: the job, its results and its timer stay
in the database untouched, so the player keeps what they crafted. What they lose
is the craft appearing on that building. Losing a craft's UI is strictly better
than losing the account.

## Finding the real cause

The next step is a client-side answer, not another server experiment — the
server side is exhausted. The reproduction is cheap and reliable, so:

1. Copy a character with a town to a throwaway row, bind the emulator to it.
2. Add one craft job pointing at a real building → stalls. Change only the
   `buildingId` → loads.
3. Instrument the client at that point (the town-build coroutine) and find what
   returns null.

The fork's own note on report #34 says `GetCraftingStation` returning null makes
the town-build coroutine never finish. That is the same shape and the obvious
first thing to check — but the crafting type here is valid for that building, so
if it is that path, the lookup is failing for a different reason.
