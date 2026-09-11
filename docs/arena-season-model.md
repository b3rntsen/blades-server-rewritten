# Arena seasons, trophies and the match reward

How the arena's season lifecycle and scoring work on this server, where every
number came from, and how to run a season rollover.

**Evidence grades used throughout:**

| grade | meaning |
|---|---|
| **Class 1** | Verbatim shipped game data — a value Bethesda authored and shipped inside the client. |
| **Class 2** | Derived from captured retail traffic. Real observations, but a reading of them. |
| **Class 3** | Modelled. Not in any asset or capture; a choice we made, and labelled as such. |

---

## 1. The headline: the scoring formula was shipped all along

Before this change, `arena_ladder.rs` said the match-reward formula "is in no
capture and never will be", and implemented a Class-3 fit: gold and XP
interpolated between anchors read off reassembled victory cards, and trophies
moved by an Elo with a flat, invented `K = 60`.

That was wrong in a specific and recoverable way. The formula is not in any
*capture*, but it is in the *client*. `reference/game-defs/loot.json` is an export
of the client's `Matchmaking` ScriptableObject — and the extractor that produced
it kept only `arenas` plus four `tuning` keys. It dropped six fields:

```
_eloFactors                 _pvpExperienceRules
_eloResultScore             _pvpSoftCurrencyRules
_trophyCountAdjustment      _trophyEquivalence
```

Those six are the arena's entire scoring and economy. They are recovered by
`script/extract_pvp_matchmaking.py` (the `common` asset bundle ships embedded
type trees, so plain UnityPy reads it) into
`server/src/arena/pvp_matchmaking.json`, and generated into const tables by
`script/gen_pvp_tuning_rs.py` → `server/src/arena/pvp_tuning.rs`.

### Regenerating

```bash
python3 script/extract_pvp_matchmaking.py \
    --bundle /tmp/blades-apk-extract/assets/Bundles/common
python3 script/gen_pvp_tuning_rs.py
cargo test -p server --locked
```

The JSON records the source bundle's sha256; `pvp_tuning.rs` records the JSON's.
`arena_ladder::tests::const_tables_match_the_committed_json` fails if a const is
hand-edited away from its source.

---

## 2. The trophy formula

```
E     = 1 / (1 + 10^((opponent_trophies - own_trophies) / 400))
S     = ELO_RESULT_SCORE[round score]
delta = round( K(own_trophies) * (S - E) )
        floored away from zero by TROPHY_GAIN_FLOOR
```

Per-term evidence:

| term | value | grade | source |
|---|---|---|---|
| `K(own_trophies)` | 100 / 90 / 80 / 70 / 60 / 50 at 0 / 500 / 1000 / 1500 / 2000 / 2500 trophies | **Class 1** | `Matchmaking._eloFactors` |
| `S` — won 2-0 | `1.00` | **Class 1** | `Matchmaking._eloResultScore._wonEveryRounds` |
| `S` — won 2-1 | `0.92` | **Class 1** | `._wonMajorityOfRounds` |
| `S` — lost 1-2 | `0.12` | **Class 1** | `._lostMajorityOfRounds` |
| `S` — lost 0-2 | `0.00` | **Class 1** | `._lostEveryRounds` |
| floor | `1` | **Class 1** | `Matchmaking._trophyGainFloor` (also in `loot.json`) |
| logistic scale `400` | `400` | **Class 3** | *Not shipped.* `EloFactorEntry` carries only a K-factor. Textbook Elo's default, assumed. See below. |
| relative-rating shape (`S - E`) | — | **Class 2** | Implied by `_eloFactors` + `_eloResultScore` existing at all; consistent with the captured deltas. |

### The owner's two questions, answered

> "it should change from the early days to the later days"

**Yes, and it is shipped.** `_eloFactors` is a K-factor ladder keyed on the
player's own trophy count. A player at 0 trophies swings at `K = 100`; the same
result in arena 6 swings at `K = 50`. The same 2-0 win against an even opponent
is worth `+50` early and `+25` late. The old flat `K = 60` was right for nobody —
it sits between the 2000 and 2500 bands and is wrong everywhere else.

> "it depends also on relative scores of a match"

**Two ways, both now implemented.** The *ratings* are relative through `E`
(beating a stronger player is worth more). And the *round score* is relative
through `S` — retail did not score a best-of-three 1/0. A 2-1 win banks 0.92 and
a 1-2 loss still banks 0.12, so a close match moves fewer trophies than a sweep.
The old implementation had neither.

### Was there an early-vs-late change *to the algorithm itself*?

**We could not establish one, and we did not invent a date.** What we can say:

* The shipped client (the final build, `common` bundle sha256
  `b8bbd3c5…28ac55`) carries exactly one set of Elo tables. There is no versioning,
  no date gate, no second table.
* Only one APK is held, so there is nothing to diff against an earlier build.
* No captured response carries scoring parameters — the server computed the swing
  and sent only the result.

So the honest answer is: *the K-factor changes with a player's progress within a
season, which is a real "early vs late" effect and is shipped; whether Bethesda
ALSO retuned the algorithm between eras of the live game is unknown.* Anyone who
finds an older APK can settle it by re-running the extractor against it.

Because it is unknown, `SeasonConfig` carries the variant so a season can be run
either way without touching the scoring code:

```rust
pub enum ScoringVariant {
    Shipped,        // banded K x round-score-weighted Elo   [Class 1]
    FlatK(i64),     // one K for everybody, result is 1 or 0 [Class 3]
}
```

### Why flat `K = 60` is not merely unsupported but wrong

`arena_ladder::tests::flat_k_60_is_impossible_for_the_captured_loss` is the load
-bearing test. From the prod capture snapshot
(`blades-snapshot-20260607-112415.db`, op49 `character` blocks reassembled out of
`arena_udp_frames`, session 168): a character on **51** trophies with
`numberPvpMatchPlayed` 3 → 4 drops to **9** — a swing of **−42**.

On a loss the swing is `K·(E − S)` with `S ≤ 0.12`, and `E` is largest when the
opponent is *weakest*. Opponent trophies are never negative, so
`E ≤ 1/(1 + 10^(−51/400)) = 0.5728`, and flat `K = 60` tops out at
`60 × 0.5728 = 34`. It cannot reach 42 against any legal opponent. The shipped
`K = 100` band reaches it against an opponent inside the shipped pairing window —
an ordinary match, which the test asserts.

A second test uses two characters' first match of a fresh season (`+57` and `+49`
from zero). That one is weaker and says so: a win's opponent can be arbitrarily
strong, so it only discriminates once you accept the shipped pairing window
(`_eplToTrophyCountList` deviation `250`) as a bound on who gets matched.

The test the old code had — `assert_eq!(trophy_delta(true, 800, 800), 30)` — was
circular: `30` is `K/2` by construction. No retail number appeared in it.

### What is shipped but deliberately NOT applied

* `_trophyDiffXpBonus` / `_trophyDiffCurrencyBonus` — real fields, read into
  `pvp_tuning`, unused. No asset or capture says what trophy gap triggers them,
  and every observed card is reproduced exactly without them. Applying them would
  mean inventing the threshold.
* `_trophyCountAdjustment._matchPlayedToTrophiesModifier`
  (`[100,100,80,60,40,30,30,20,20,20,20]`, indexed by matches played) — shipped,
  read in, unused. Its name and its neighbour `_eplToTrophyCountList` both point
  at the *matchmaking search window* (a provisional-rating widening that shrinks
  as a player logs matches), not at the trophy delta. It is not multiplied into
  anything.
* `_trophyEquivalence` — 100 identical degenerate rows
  (`expected_trophy_count: 1, max_disparity_percentage: 0.0`) in the shipped
  build. Preserved for provenance; nothing reads it.

---

## 3. The match reward (gold and XP)

```
gold = base_currency(level)
     + currency_bonus_per_round_won(level)[rounds_won]     # index 0 is NEGATIVE
     + arena_currency_bonus(level)[arena - 1]
     + win_currency_bonus_2_to_0 | win_currency_bonus_2_to_1   # winner only
xp   = the same shape over the experience row
```

All **Class 1** (`Matchmaking._pvpSoftCurrencyRules` / `._pvpExperienceRules`,
100 rows each, one per character level).

The third input — **the arena** — is what the old fit was missing, and it is why
that fit could never close. Same character, same round score, two different
payouts (14 413 and 14 654 gold at L72 2-0): those are arena 1 and arena 2, and
the difference `+241` is exactly `arena_currency_bonus[1] − arena_currency_bonus[0]`
at level 72.

**Result: every observed retail card value reproduces to the unit.** 23 gold
values and 19 XP values, where the fit's worst case was 6.1%. Two consequences
worth noting:

* The old model's L56 "soft anchor" was **wrong**. It read simi's single card as a
  2-0 WIN and back-solved a base of `1426` from it. The shipped table says the
  card is a **1-2 LOSS in arena 1** — and independently puts the same card's XP
  (252) on that exact row. Two quantities agreeing on one row settles it.
* One observation still does not fit: a L72 2-1 win recorded at **417** XP where
  the shipped row pays **427**. No arena index closes a 10-point gap. Every other
  value in the family lands exactly, so the likeliest cause is a mis-attributed
  level or round score in the ENet reassembly that produced the card — but those
  scripts are gone, so it is recorded as an open discrepancy
  (`the_single_unexplained_xp_observation_is_documented_not_hidden`) rather than
  explained away.

---

## 4. The season model

`server/src/arena/arena_season.rs`.

### Shape — Class 2, capture-derived

`pvpSeasonHistory` is a **map from season UUID to a frozen copy of the eleven live
PvP counters**. Verified against the prod snapshot: 5 088 responses carrying the
field, 9 characters, **61 distinct season UUIDs**.

```json
"pvpSeasonHistory": {
  "ea65edb8-35e1-4328-b36c-e3138300208c": {
    "highestArenaReached": 3, "highestLevelArenaReached": 5,
    "highestLevelArenaReachedTimeSecs": 1748500000,
    "matchmakingPvpTrophies": 1215, "numberPvpMatchPlayed": 219,
    "pvpChestMeter": 2, "pvpExceptionEasierMatchRemaining": 0,
    "pvpExceptionHarderMatchRemaining": 0, "pvpTrophies": 1215,
    "pvpWinningStreak": 2, "trophyCountModifier": -40
  }
}
```

`character.pvpSeasonId` names the season the live counters belong to. It was
already modelled on `CompleteCharacter` and, until now, never read or written by
anything.

### Cadence — Class 2

The 61 archived seasons run 2020-10 → 2026-05 and are **calendar monthly**. Taking
`highestLevelArenaReachedTimeSecs` as a within-season timestamp, no two seasons'
observation windows overlap and every window ends on the last day of a month or
the first of the next:

```
4df8ebe3…  2026-02-18 .. 2026-03-01
177b6c32…  2026-03-03 .. 2026-04-01
ca35eb60…  2026-04-23 .. 2026-04-28
ea65edb8…  2026-05-05 .. 2026-05-31
```

Day-level resolution only. The captures bound the boundary to the turn of the UTC
month; they do not pin the exact instant or Bethesda's time zone. A 2026-09-01
start is on the retail cadence.

### Reset — Class 2

A rollover **zeroes everything**:

| evidence | what it shows |
|---|---|
| `38c987fd` archives `numberPvpMatchPlayed` 159 / 309 / 272 / 219 across four consecutive seasons, and is at 45 in the live one | the match counter is per-season, not lifetime |
| `128f1c2a` and `97cf5fa6` each have `numberPvpMatchPlayed == 1` in the live season with `pvpTrophies == matchmakingPvpTrophies` (49 and 57) | one match took them from zero |
| `ee5b1920` archives season `ca35eb60` as an all-zero block, `highestArenaReached: 1` / `highestLevelArenaReached: 1` / `…TimeSecs: 0` included | the ladder rung resets too, not just the counters |

No shipped asset states the rule *as a rule*. `PvpSeasonsData._trophyResetRule`
exists in `dump.cs` as a `string` field, but the `PvpSeasonsData` asset is not in
any APK bundle we hold — it was server-side data. So `TrophyResetRule` carries
**one** variant, the one the captures show. A soft or decayed reset is
deliberately not offered rather than invented.

### The 2026-09 season

```rust
SEASON_2026_09 = SeasonConfig {
    id:         9b3f1c74-5a20-4f8e-9d61-2c07ab54e319,  // ours, freshly minted
    number:     1,                                      // of THIS server
    start_unix: 1_788_220_800,   // 2026-09-01T00:00:00Z
    end_unix:   1_790_812_800,   // 2026-10-01T00:00:00Z
    scoring:    ScoringVariant::Shipped,
    reset:      TrophyResetRule::HardReset,
};
```

The UUID is ours, not one of the 61 retail ones, so a transferred retail
character's history can never collide with it. `number` is a local count: retail's
own numbering is **not recoverable** (`pvpSeasonHistory` is an unordered map and
no captured response carries a season index), so printing a retail season number
would be a guess.

---

## 5. The announcement

Generated from the season config by `arena_season::season_announcement`, and
merged into the captured retail list at serve time by
`announcements::get_announcements`. There is no hand-edited duplicate in
`announcements.json`, so the entry's dates cannot drift away from the season's.

The record shape is the captured one verbatim — five keys, nothing more:

```json
{
  "assetUrl": "https://announcements.blades.bgs.services/2026/09/01/<id>",
  "id": "<id>",
  "startTime": 1788220800,
  "ttl": 1788307140,
  "type": "BASIC"
}
```

All 156 captured records are `"type": "BASIC"`, and 155 of them run for exactly
`86 340` seconds (one day less a minute), which is what `ANNOUNCEMENT_TTL_SECS`
reproduces. The client filters by `startTime`/`ttl` itself, so the entry is inert
until the season opens.

> **Open item for a human.** No banner image is shipped. The `assetUrl` points at
> a host this server already answers on (`status.rs` serves
> `announcements.blades.bgs.services/status/status.json`), but nothing serves that
> path. The client will list the news item and quietly fail to load its artwork —
> exactly what it already does for all 156 replayed retail entries. Dropping a PNG
> at that path fixes it.

---

## 6. DB-backed season close and reward controls

The owner-facing lifecycle uses the dev-token-gated `arena-seasons` routes:

1. create the next season as `scheduled`;
2. dry-run `POST /arena-seasons/{current}/end` with its `nextSeasonId`;
3. repeat with `"apply": true` to freeze the ladder, record awards, archive/reset
   every character, end the current season, and activate the next one;
4. dry-run `POST /arena-seasons/{ended}/grant-awards` with an explicit reward
   catalogue, then apply it in small batches until `pendingAfter` is zero.

The dry run validates that the selected next season exists and is still
scheduled. The applied close is one Postgres transaction. A missing/ended next
season, a failed standings insert, or a failed character reset rolls the whole
close back; the current season remains active and the request can be retried.
Transaction-local limits cap lock waits at one second, individual statements at
ten seconds, work memory at 1 MB, and temporary files at 4 MB.

Standings come from `arena_match_results` between the season's `starts_at` and
`ends_at`, not lifetime totals or mutable current character JSON. The final audit
row before the cutoff supplies cups and the sticky Arena high-water. This means a
player who finishes on zero cups still receives their highest-Arena participation
award. Player leaderboard awards use the 1 / top-3 / top-10 / top-50 / top-100
tiers; later retail's guild rule is one flat `top100` tier for every member of a
qualifying guild.

Award granting is deliberately separate. The base APK contains the
`PvpSeasonRewards` / `PvPResetRewards` schema, but not the downloaded runtime
prize table, and retail changed prize contents between seasons. The endpoint
therefore accepts explicit keys such as:

```json
{
  "apply": false,
  "limit": 5,
  "rewards": {
    "guild_rank:top100": {
      "currencies": { "<currency-uuid>": 100 },
      "stackableItems": { "<item-template-uuid>": 1 }
    },
    "arena_reached:arena2_level4": {
      "chests": [{ "tier": 4, "level": 50 }]
    }
  }
}
```

That example illustrates the wire shape only; it is not a claimed retail prize
table. A dry run reports every `missingRewardKey`. Apply is restricted to ended
seasons, defaults to five awards, clamps at 25, locks with `SKIP LOCKED`, and marks
each award with `granted_at` in the same transaction as its character economy
update. Grant transactions use a 500 ms lock timeout and three-second statement
timeout. Repeating a batch cannot pay an award twice.

---

## 7. Legacy compile-time rollover

`POST /blades.bgs.services/api/dev/v1/arena-season-rollover`, dev-token gated like
the rest of `admin.rs` (`Authorization: Bearer $ARENA_IMPORT_TOKEN`).

**It defaults to a dry run.** A missing or mistyped `apply` reports what would
happen and writes nothing.

```bash
# 1. See what it would do. Always do this first.
curl -sS -X POST https://<host>/blades.bgs.services/api/dev/v1/arena-season-rollover \
     -H "Authorization: Bearer $ARENA_IMPORT_TOKEN" \
     -H 'Content-Type: application/json' -d '{}' | jq

# {
#   "applied": false,
#   "seasonId": "9b3f1c74-5a20-4f8e-9d61-2c07ab54e319",
#   "seasonNumber": 1,
#   "charactersSeen": 412,
#   "charactersReset": 407,
#   "charactersArchived": 380,
#   "charactersAlreadyCurrent": 0,
#   "charactersUnreadable": 5,
#   "highestArchivedTrophies": 2118
# }

# 2. Only when the numbers look right:
curl -sS -X POST .../arena-season-rollover \
     -H "Authorization: Bearer $ARENA_IMPORT_TOKEN" \
     -H 'Content-Type: application/json' -d '{"apply":true}' | jq
```

Per character it archives the live PvP block into `pvpSeasonHistory` under the
season the character was in, zeroes every live counter, and stamps the new
`pvpSeasonId`.

Properties worth knowing:

* **Idempotent.** A character already stamped with the target season is skipped,
  so re-running after a partial failure resumes rather than re-wiping.
* **Unreadable rows are skipped, never written.** `charactersUnreadable > 0` means
  some `character` JSONB would not deserialize; investigate before assuming the
  run was complete.
* **A nil `pvpSeasonId` is not archived.** A character that never carried a season
  gets stamped and zeroed but produces no history key, rather than putting a
  `00000000-…` entry in the client's end-of-season screen.
* **No migration.** Everything lives in the existing `characters.character` JSONB.

### Timing

This has **not** been run against production, by design. It should run once, at or
just after `2026-09-01T00:00:00Z`. Nothing schedules it — it is a deliberate,
observed operator action, because it zeroes every player's trophies.

---

## 8. Things a human should verify

1. **The logistic scale of 400.** Class 3. Not shipped, not constrained by any
   capture. It only affects how fast the swing decays with the rating gap, never
   the magnitude at an even match (that is `K` alone), so it is low-risk — but it
   is a choice.
2. **The pairing-window reading.** `_eplToTrophyCountList`'s `deviation: 250` is
   read as a matchmaking window. The second falsification test leans on it; the
   first does not.
3. **`_matchPlayedToTrophiesModifier`'s actual role.** Shipped and unused here. If
   it turns out to scale the trophy delta rather than the search window, new
   players' swings are wrong by up to 5x.
4. **The season boundary instant.** Captures pin it to the turn of a UTC month at
   day resolution, not to the second, and not to a time zone.
5. **The season *number*.** `1` is this server's count, not retail's continuation.
   If the owner wants "Season 62", that is a product decision, not a data one.
6. **The banner image** (section 5).
7. **The unexplained 417 XP observation** (section 3).
