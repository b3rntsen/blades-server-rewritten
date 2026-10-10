# The craft/town loading hang

**Status: root cause found in the client binary (2026-10-10); the blanket
containment is gone.** Crafts are served on their real building again, as retail
did. Only a job whose results name something that is not an item template is
still held off its building, because that is the actual trigger.

## Symptom

The client completes its entire API boot — sync, characters, wallets,
inventory, towns, quests, shops, every call a 200 in a few milliseconds — then
sits on the title screen forever. No error, anywhere: not in logcat, not in the
server log, not in the transcript. The process keeps rendering at ~60% of a
core, so it does not look hung from outside.

Players report it as "can't connect", which is what makes it expensive: it
reads as a VPN or account problem and gets triaged as one.

## Root cause

A craft result whose item template the client does not have.

Read from `libil2cpp.so` (base APK, sha256 `9fc19d29…`; RVAs from
`reference/il2cpp/dump.cs` in blades-capture, decompiled with Ghidra):

1. `FindCraftingStationParser.Parse` (0x24E10E0) writes each `/crafts` entry
   into the town's persistent tree, under the building its `buildingId` names
   and the station its `craftingTypeId` names. If no building has that id,
   nothing is written.
2. While the town builds, each building's `CraftingStation.RetrievePersistent`
   (0x1E53F3C) restores its craft: recipe, stack size, then **every element of
   `results`** as `new StackedItem(node)`, then the timer.
3. `StackedItem.RetrievePersistent` (0x1C7844C) builds an `Item` and calls
   `Item.RetrievePersistent` (0x1F2C02C), whose first act is
   `ItemTemplateManager.GetTemplateByID(templateId)` (0x1F2BA78) — a bare
   `Dictionary<,>` indexer. An unknown id throws `KeyNotFoundException`, and
   the town-build never finishes.

For contrast, `RecipeManager.GetRecipeByID` (0x1DDFE88) checks `ContainsKey`
first and returns null, so an unknown `recipeId` is harmless.

The jobs that stalled carried `results: {"stackableItems": {"<recipe id>": 1}}`
— the fake result the unknown-recipe path minted from 2026-07-04 until
132e20ba (2026-09-17). A recipe id is never an item template (0 of 2,978 APK
recipes collide with the 1,113 templates in `parsed.json`).

That explains every earlier measurement:

| observation | why |
|---|---|
| `buildingId` → real building: **stalls**; → unresolvable uuid: **loads** | step 1: no building, no station, nothing retrieves the bad template |
| an in-progress craft stalls as well as a finished one | step 2 runs for any job with results |
| recipe, crafting type and building `state` did not matter | they were varied while the results were held constant — and the results were the bad part |
| the 2026-08-19 relabel hang (Alchemy → Smithing on a Forge) | an Alchemy row on a Forge has no station to land on; relabelled to Smithing it lands on the Forge's station, which then retrieves the recipe-keyed stackable |
| tickets #175–#180 ("the issue was a stackableitem") | same fake result |

## What the server does now

`crafts_for_client` in `server/src/craft.rs`:

* Serves every job on its real building, finished or not, and pays nothing out
  on read. Retail did exactly this: the same finished Prime Elixir sat on its
  Alchemist in 27 captured reads over nine days, and the client collected it
  with `POST /crafts/{id}/finish`.
* Rebuilds a legacy fake result into the real item when the APK knows what the
  recipe makes (`repaired_craft_fields`), whatever bench the job names.
* Detaches (stable stand-in `buildingId`, row untouched) only a job whose
  results still name a non-template after that repair
  (`results_resolve_in_client`).

Retail fixtures for all of this are in the `crafts_stay_on_their_building`
tests.

## Verifying on the client

`tools/harness/client-gate.sh boot --checkpoint wolfwalker-250` (blades-capture)
against a server built from this change, after starting a craft at the Forge and
relaunching the app: the gate must reach the hub, and the Forge must show the
running craft. `tools/harness/faults/craft-poison.sh inject` is still the red
control: it puts a recipe-keyed stackable on the Forge's Smithing station, which
is exactly what the server now refuses to emit.
