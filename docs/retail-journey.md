# The retail journey — fixtures, findings, and how to find the next gap

The "journey" is the single-player loop the server exists to serve: create a
character, play the main and side quests, spend the experience, and build and
upgrade a town. This doc is what the capture corpus says that loop actually did,
which parts of it we now reproduce, and — the part that keeps paying — the
method for finding the parts nobody has looked at yet.

Everything labelled MEASURED was counted over the pre-shutdown corpus on the
capture host (`api_captures` + `blades-archive.capture_bodies`), filtered to
`https://blades.bgs.services/`. Our own emulator answers on `127.0.0.1:8087` in
the same table; treating those as ground truth is circular and has produced a
wrong conclusion here before.

---

## 1. The fixtures

`script/extract_journey_fixtures.py` derives everything below into
`deploy/retail-journey/`. Nothing in that directory is authored; every value is
read out of a captured Bethesda response, and the tests replay the observations
rather than asserting a summary of them.

```sh
# on the capture host, read-only (bodies live in the sibling archive db)
sudo python3 script/extract_journey_fixtures.py \
    --db /var/lib/newblades/db/blades.db \
    --archive /var/lib/newblades/db/blades-archive.db \
    --out /tmp/retail-journey
```

| file | what it holds | read by |
|---|---|---|
| `levelups.json` | 37 level-ups, levels 1 → 38, with the character either side | `blades_lib::features::level_up` |
| `town_ops.json` | 357 town mutations with gold / town-XP deltas | `server::town::when_the_town_is_paid` |
| `style_prestige.json` | the town XP each restyle paid | same |
| `spawn_levels.json` | 66,994 spawns as `(playerLevel, enemyLevel)` + XP per enemy level | `blades_lib::util::quest::how_retail_scaled_enemies` |
| `quest_completions.json` | 176 quest rewards, resolved to template ids | reference |
| `objective_rewards.json` | 21 reward-bearing `/objectives` responses | reference |
| `endpoint_coverage.json` | 88 endpoints retail answered, across 40 players | `server::route_registration::retail_coverage` |

`--user` defaults to the one player whose 57,883 requests span the whole journey.
The spawn and coverage tables deliberately ignore it and read **every** captured
player: how retail scaled its enemies is not a property of one player, and §4
below is the story of what reading one player closely still misses.

---

## 2. What was wrong, and what it is now

### Experience was earned and never spent

`/levelup` incremented the level and left `character.experience` alone, so it only
ever grew. The client reads its xp bar as the remainder against the next
threshold, so the bar drifted further from the truth at every level, and nothing
stopped a client asking for a hundred levels in a row.

MEASURED, 37 of 37: `experienceAfter == experienceBefore -
level_rewards[newLevel].xp_to_reach`, exactly. Level 1 → 2 with 57 xp banked
answers with 7, because level 2 costs 50.

The same captures settle the response shape. Retail sends `wallet` only when the
level credited a currency, and then only that currency — gold at levels 2, 4, 9,
17, 25 and gems at 3, 7, 10, 13, 18, 23, 29, with no `wallet` key at all on the
other 18. We sent the whole purse every time.

### Quest XP was 23× too generous

`givenXp` was `100 * enemyLevel`. Retail's own table pays **220** at enemy level
50; we paid **5,000**. Quest kills are most of a character's progression, so that
alone made levelling roughly an order of magnitude too fast — and it is what makes
spending experience at `/levelup` mean anything at all.

`quests_daily.json` already carried the real table for levels 1–25, deliberately
unwired. It now covers 1–90 and is read. The corpus agrees with every one of the
25 rows it already had.

### Quests got harder than retail exactly where a player could do least about it

Enemy level was the player's own, all the way to 100. Retail's enemies track the
player to about level 45 and then fall behind, reaching a median **22 levels
below** at player level 100.

Scored against controls, because a median over every spawn group is closer rather
than right: the shipped curve reproduces 20.6 % of captured spawns exactly and
61.4 % within two levels, against 6.2 % / 26.2 % for enemy = player and 4.3 % /
15.4 % for a flat `player − 10`. The medians are made monotone with a running
maximum first — a level-up that makes the world easier is a worse artefact than
the 0.9 points of accuracy it costs.

> **The per-spawn-group cap this repo planned for does not survive measurement.**
> `quests_daily.json` said to mine it: "for a group observed at player levels well
> above its cap, the max enemyLevel seen IS the cap." 188 of 1,562 groups are
> unambiguously capped by that test — 90 %+ of their over-cap observations pinned
> at the ceiling, across three or more player levels — but they account for
> **0.3 %** of all spawns at player level 60 and above. Shipping them would change
> nothing where the gap actually is. `min(playerLevel + offset, perGroupCap)`
> explains 15.2 % of spawns against a 6.9 % no-cap control: better than nothing,
> and not a model.

### The town was paid when a build was ordered, not when it finished

MEASURED over 357 town mutations: retail moves `town.levelInfo.experiencePoints`
on `…/complete` and on nothing else. 29 places, 67 upgrades and 23 destroys moved
it by exactly zero. We paid at `place`/`upgrade`, so an upgrade that was ordered
and never finished still banked its prestige — and destroying and re-placing
banked it again. The town level ran ahead of the town.

What a completion pays is the level's own `prestigeForLevel`, with the style's
**not** added on top; adding it (which `lookup_level_cost` does) makes every one
of the 72 matching completions a 10 % overpayment.

And a **restyle pays**, on all 124 captures, where we paid nothing. Town level
gates building upgrades through `requireTownLevel`, so that shortfall compounds
into buildings a player can never reach. The grant depends only on
`(buildingType, level, newStyle)` — the same change pays the same in both
directions, so it is not a difference from the style being replaced. (Retail
therefore pays again every time a style is flipped back and forth, which is what
the captured player spent four minutes doing. Reproduced deliberately.)

### A player could not shop in a friend's town

`…/social/users/{u}/characters/{c}/shops/{s}` and its `/purchase`: 1,673 captured
requests from nine players, no route. See §4 — this one is the argument for the
whole section.

---

## 3. Known-wrong numbers we did not invent a replacement for

* **TownHall prestige above level 0.** `building_upgrades.json` authors
  `prestigeForLevel: 0` for TownHall levels 1–10, while retail paid 525 for the
  corpus's very first completion. One observation, so the completion test pins it
  as the single permitted disagreement rather than patching the table around it.
* **Wall and gate restyle prestige.** The measured grants are a fixed 65 (walls)
  and 115 (main gate) below `styleInputs[].prestigeForLevel`, which is exact for
  the TownHall. The table's shape is right and one of its numbers is not;
  `style_prestige.json` carries measurement where we have it and the table stays
  as the fallback.
* **The second XP population.** Each enemy level carries two `givenXP` values (14
  and 41 both appear at level 12). The canonical value is the maximum; what
  produces the lower one is not identified, and a control that separates
  assist-kills from a second enemy tier has not been run.

---

## 4. How to find the next gap

The method that produced this work, in the order it pays.

**1. Diff the endpoint inventory against the routes, across every player.** This
is now `route_registration::retail_coverage`, and it is the cheapest gap-finder in
the repo: normalise every URL retail answered, and require a route for each. It
is what found the social shop. Nine players used it and the one whose journey was
being read closely was not among them — no amount of care reading one corpus
would have surfaced it. Anything deliberately unserved goes in `NOT_SERVED` with a
reason, and a second test rejects an excuse naming an endpoint retail never sent.

**2. Reconstruct the timeline, not the endpoint.** Most of §2 came from walking
the captures in order and diffing the state either side of one call — experience
before and after a level-up, gold and town XP before and after a build. A single
response says what a field *is*; a pair says what the call *did*. The extractor
records the guards with each observation (`pendingQuestTownXp`, `buildingKnown`)
so the tests can drop the ones where something else moved the same number, rather
than the extractor quietly averaging them away.

**3. Every probe needs a control.** "Absent" and "I looked in the wrong place"
look identical. `enemyGeneratedData` is a map keyed by spawn group, not a list;
the first pass at §2 read it as a list, counted zero spawns, and would have
reported the field as uncaptured. Score a candidate rule against the alternative
you are trying to beat and against a deliberately wrong one — the enemy curve is
only worth shipping because `enemy = player` and `player − 10` both score far
below it on the same data.

**4. Mutate the test before believing it.** Every fix here was checked by undoing
it and requiring the test written for it to fail. A compile error is weak
evidence; `script/verify_quest_tests_are_red.py` is the existing tool for this.

**5. When the corpus cannot answer, say which source can.** In rough order of how
often it works: the IL2CPP dump (`reference/il2cpp/dump.cs` in blades-capture) for
structures, enums and constants; the raw APK bundles for authored tables —
`building_upgrades.json`, `item_durability.json` and `job_pools.json` all came
from there, and the APK is how the wall/gate discrepancy in §3 should be settled;
another player's captures, which is what §4.1 exploits; and the client's own
localization tables for what a feature is called when nothing else names it.
Leaving a number unfilled and logged beats synthesising one — a constant wearing a
fallback's clothes is indistinguishable from a measurement once it is in the file.

### What the corpus still cannot settle

* `gameEventQuestsFinished` — no field separates a finished instance from an
  active one. Needs a fresh capture, which no longer exists.
* The dungeon `currentState` / `dungeonInstance` binary, which is opaque and only
  needs decoding to *validate* combat. Quest combat is client-simulated; the
  server is a save-game and a loot oracle.
* Craft input costs, respec cost, inventory-upgrade gem cost — all lenient, all
  flagged in `docs/non-arena-feature-gaps.md`, none captured.
