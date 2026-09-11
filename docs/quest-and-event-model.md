# Quests, daily jobs and event ("Sigil") quests

How the three quest-shaped systems work, which numbers come from captured retail
traffic, which are authored, and the one modelling mistake that keeps recurring.

Everything below that is labelled MEASURED was counted over the pre-shutdown capture
corpus on the capture host (`api_captures` + `blades-archive.capture_bodies`),
filtered to `https://blades.bgs.services/` — retail traffic only. Our own emulator's
traffic sits in the same table under `http://127.0.0.1:8087/` and is excluded on
purpose: treating our own answers as ground truth is circular, and it has already
produced one wrong conclusion (see "The `gameEventQuests` mix-up").

---

## 1. The three systems

| | source of truth | where it is served | rotation |
|---|---|---|---|
| **Quests** (story / side / bounty / guild / arena) | `parsed.json` `quests` — 172 entries | `quests[]` in `POST /quests` | none; accepted per character |
| **Town jobs** | `job_pools.json` (APK `JobData`) | `jobs[]` + `jobPools[]` | daily 05:00 UTC, weekly boss/featured pools |
| **Event quests** ("Sigil") | `game_events.json` + `event_quests.json` | `gameEventQuests[]` / `gameEventQuestsInWarning[]` | every `recurrenceInterval` days, open for `durationSecs` |

They are *not* one mechanism. The often-repeated shorthand "daily quests and sigil
quests are one system — the Events system drives both" is half right: the **Sigil**
quests are the Events system, and the **daily** rotation is the town-JOB board in
`job_pools.json`. What is genuinely shared is the 05:00-UTC reset boundary.

---

## 2. The `gldQuestId` gotcha

**An event quest has two ids and only one of them means anything.**

```jsonc
{
  "questId":    "298c48b8-9bcc-45ca-948b-234de2cbb202",   // per-CHARACTER instance
  "gldQuestId": "7f07d85f-f4ed-4762-b670-79e36b224902",   // the TEMPLATE
  "type": "GAME_EVENT",
  "gameEventQuestData": { "gameEventInstanceId": "ffcbe281-…::1777694400" },
  "rewards": [ …five milestones… ],
  "finalReward": { … }
}
```

The `questId` resolves to **nothing** — not a quest in `parsed.json`, not a key in any
static table. Only `gldQuestId` does. So the objectives, the dungeon, the wire
`version` and the rewards must all be looked up under `gldQuestId`.

MEASURED: 1271 captured `gameEventQuests[]` entries had `questId != gldQuestId` and
none had them equal. Every `NORMAL` quest, by contrast, has them equal (1315/1315) —
which is why code written against ordinary quests silently "works" until an event
quest arrives.

Two concrete failures this caused here:

* `quest_rewards.json` was keyed by whatever id sat in the captured `/complete` URL.
  For an event quest that is the instance, so **78 of its 148 keys** named instances
  that will never exist again, and every event quest paid nothing. Resolving them
  back through `gldQuestId` is what made the table usable.
* `generate_quest_data` sets `gld_quest_id = quest_id`, so the fallback chain
  `quest_rewards.get(quest_id).or(quest_rewards.get(gld_quest_id))` could never take
  its second branch. It read as defensive and was dead.

---

## 3. Event recurrence

`game_events.json` — 39 events, all capture-derived:

```jsonc
{ "eventId": "b483c668-…", "questId": "7f0d1508-…",  // questId here IS the gldQuestId
  "important": true, "instanceDurationSecs": 172800,
  "recurrence": { "recurrenceType": "daily", "startTimeSecs": 1663214400,
                  "durationSecs": 172800, "recurrenceInterval": 39 } }
```

An instance opens at `startTimeSecs + k * recurrenceInterval * 86400` and stays open
`durationSecs`. Its id is `"<eventId>::<startTimeSecs of that instance>"`.

`recurrenceType` says `"daily"` for all 39 and is a red herring — the interval field
is what governs. All 39 events use interval 39 days and a 172 800 s (2-day) window.

The arithmetic is a useful self-check: 39 events × (2/39) ⇒ **2** expected open at
any instant, and 39 × (1/39) ⇒ **1** within a day of opening.

MEASURED, and it matches: retail's `/quests` responses carried 2 entries in
`gameEventQuests[]` in 614 responses and 1 in 43; `gameEventQuestsInWarning` had
exactly 1 entry in all 686 responses that had any.
`blades_lib::features::game_events` asserts both against the committed file.

### The three arrays

| array | meaning | MEASURED |
|---|---|---|
| `gameEventQuests` | window covers now | 1271 entries, all `GAME_EVENT` |
| `gameEventQuestsInWarning` | opens **soon** — not "ends soon" | 686 entries, `start - now` between 0.1 h and 24.0 h, always exactly one |
| `gameEventQuestsFinished` | **not implemented** | 101 entries, see below |

`gameEventQuestsFinished` is deliberately left empty. Every captured entry sat 1–48 h
*after* its instance start — i.e. inside the same 48 h window the active array uses —
and carried `completed: false`. So it is neither "the window elapsed" nor "the player
finished it", and nothing else in the payload separates it from an active entry.
Rather than guess, we send nothing: a wrong guess puts quests on a player's finished
list that retail would not have. If someone finds the discriminator, the corpus query
is in `script/extract_quest_data.py`.

---

## 4. Rewards

### Ordinary quests — flat, from `quest_rewards.json`

Keyed by **template** id. Regenerated by `script/extract_quest_data.py`; every value
is a verbatim `reward` object from a captured `/quests/{id}/complete` response.

Coverage: **116 of the 172** quests in `parsed.json` have a flat reward backed by
captures or the matching APK reward preview. Adding the 39 event templates (below)
brings the total to **155 / 172 (90 %)**.

The other 17 have no non-zero flat payout or event ladder. **They pay nothing and log
a warning.** Some quest assets explicitly preview zero; where a retail value is truly
missing, no number is synthesised. Observed `characterXp` spreads across 200–900 with
no rule that predicts it from level, category or objective count, so a constant would
be a fabrication wearing a fallback's clothes.

18 quests were observed with more than one reward payload. The most-frequent one is
used and the alternatives are kept in `quest_rewards_variants.json`.

### Event quests — a five-step milestone ladder

An event-quest instance is **repeatable five times**, and each completion pays a
different, increasing milestone.

* The Nth completion pays `rewards[N]`.
* The last additionally pays `finalReward`, merged into the same `/complete` body.
* A sixth completion pays nothing.

MEASURED across 93 retail instances and 300+ completions: 91/93 first completions
matched `rewards[0]`, 67/68 the second, 59/60 the third, 56/57 the fourth, and **all
54** observed fifth completions paid `rewards[4]` merged with `finalReward`.

The count lives in `ServerState.event_quest_completions`, keyed by instance quest id
— deliberately off the wire, because retail sends no such field and the quest body is
serialized straight to the client.

`event_quests.json` carries, per template: the objective ids, the wire `version`, the
five `rewards` in the form the client displays, the `finalReward`, and
`payableRewards` — the same five milestones in the form `/complete` actually granted
them. Retail lists the gem currency under `stackableItems` in `rewards[]` and under
`currencies` in the `/complete` body; rather than re-deriving which uuid is a
currency, the granting form is taken verbatim. All 39 templates have all five
observed.

Retail rotated the event token per season, so a template appears with several reward
payloads across the corpus. The most-frequently-observed one is used and the rest are
kept under `_meta.rewardsVariants`, so the choice is auditable rather than hidden.

### Objective rewards

20 of the 301 objectives in `parsed.json` carry a `rewards[]` entry, which is the
population behind retail's 42 reward-bearing `/objectives` responses. `experience`
and `town_points` are granted. `items_to_reward` is **not**: 18 of those 20 name an
item template, and turning a template id into an instanced item needs the generator
the shop/craft paths own. This is a known, bounded gap.

Before this change `/objectives` granted the **entire quest reward** whenever any
objective completed — double-paying against `/complete`.

---

## 5. The `gameEventQuests` mix-up (what this replaced)

`gameEventQuests[]` used to be filled with `type: "NORMAL"` quests picked by a
guessed daily rotation out of `quests_daily.json` — a design guess, honestly labelled
as one inside that file, that nevertheless reached real clients.

The corpus is unambiguous that this is not what the array is. Every one of the 2753
captured entries in it had `type: "GAME_EVENT"` and a `gameEventQuestData`. The 1645
`NORMAL` entries that a naive query also finds there are **our own server's output**,
recognisable by `seed: 1234` (hard-coded in `generate_quest_data`) and
`difficultyLevel == the player's level`. Filtering the corpus to retail hosts makes
them disappear.

So the array is now event-driven only, and `quests_daily.json`'s `dailyQuestPool` /
`selection` blocks are no longer consumed. Its `levelScaling` table still is, by
`generate_quest_data`.

---

## 6. What is still missing

Ordered by how much it blocks play.

1. **Town jobs are listed but not enterable.** `dungeon::enter_quest_dungeon` 404s for
   a runtime-generated job because its questId is not in `game_data.quests`. All 17
   `dungeonTemplateId`s the job generator draws from **do** exist in `parsed.json`'s
   417 dungeons, so synthesising the dungeon from `jobSetup` is tractable — it just
   was not in scope here.
2. **Enemy level has no per-spawn-group cap.** `quests_daily.json.levelScaling
   ._measured` records the real model as `min(playerLevel + offset, perSpawnGroupCap)`
   and we implement only the first half, so our quests are *harder* than retail above
   roughly level 40.
3. **`givenXp` is a modelled `100 x enemyLevel`.** The corpus has the real table
   (`levelScaling._measured.givenXpByEnemyLevel`, 11 XP at level 1 rising to 92 at
   25). Wiring it while enemy level is still wrong would change quest XP for every
   player at once, which is a balance decision, not a bug fix.
4. **`gameEventQuestsFinished`** — see §3.
5. **Objective item rewards** — see §4.
6. **12 `/quests` 500s** were logged against our server on 2026-08-23 (serviceId 1,
   errorCode 100). Cause not identified; they predate this change and are not
   explained by it.

---

## 7. Regenerating the data

On the capture host, read-only:

```sh
sudo python3 script/extract_quest_data.py \
  --db /var/lib/newblades/db/blades.db \
  --archive /var/lib/newblades/db/blades-archive.db \
  --out /tmp/quest-static
```

It writes `game_events.json`, `event_quests.json`, `quest_rewards.json`,
`quest_rewards_variants.json` and `instance_to_template.json`. The first three go to
`deploy/static/`.

## 8. Checking the tests still bite

```sh
python3 script/verify_quest_tests_are_red.py
```

Applies a mutation that undoes each fix, runs the test meant to catch it, and
requires an assertion failure. A compile error is rejected as weak evidence.
