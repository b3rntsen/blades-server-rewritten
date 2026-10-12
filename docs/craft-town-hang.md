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

Run the real client against a **local** server built from this change. Do not
use `client-gate.sh boot --checkpoint wolfwalker-250`: that checkpoint
reconnects WireGuard to production, so the client talks to prod's arena and
never sees this branch.

The local-target mode in blades-capture `tools/harness/local-target/` does the
routing. On the emulator it redirects DNS and :443 to a mitmproxy on the Mac.
Every game API call goes to the local server, bundles and announcements are
fetched GET-only from the public edge, and WireGuard stays off. Steps:

1. Build the server from this branch, then run it against a throwaway Postgres DB.
2. `local-target.sh up` and clear the app. Play the intro and let the bundles
   download. The new player gets a starter character on the local server.
3. On the local DB only, give that character a Forge, materials and gold
   (`prep-town.py`). Start a craft at the Forge.
4. Fully restart the app. The client must reach the hub. `GET /crafts` in the
   router log must show the job on its real `buildingId`. The client's
   `Save_<character>.bin` must hold the job under the Forge's Smithing
   station, and its timer must end at `completedAt`.
5. Red control: on the local DB, set the job's `results` to
   `{"stackableItems": {"<recipe id>": 1}}`. Serve it raw on the Forge from a
   build with the repair and detach turned off. The client must hang on the
   title spinner after its boot calls. Restart this branch's build on the same
   row: it must repair the result and the town must load. To show the control
   build itself is not at fault, serve the clean row from it as well; that
   must load too.
6. `local-target.sh down`. This removes the emulator redirects and the local CA.

This was run on 2026-10-11 (PR #493). All four outcomes came out as expected:
PR on the clean job loads, control on the poisoned job hangs, PR on the
poisoned job loads, control on the clean job loads.
