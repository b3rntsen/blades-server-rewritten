# Guilds

How guilds work on this server, what that is based on, and where we are guessing.

Implementation: `server/src/guild_policy.rs` (every rule, pure and unit-tested)
and `server/src/guild.rs` (routes, wire shapes, persistence).

---

## 1. Sources

Three independent sources, and they agree with each other everywhere they
overlap. That agreement is why most of this document states facts rather than
guesses.

| # | Source | What it gives |
|---|--------|---------------|
| 1 | **The shipped Unity asset.** `GuildData` + nested `GuildRankData`, read out of `assets/Bundles/BuildPlayer-common.sharedAssets` (MonoBehaviour `path_id` 454) in the game APK. | Every tunable, and the complete permission matrix. Committed at `data/guild_data.json`; regenerate with `script/extract_guild_data.py`. |
| 2 | **Captured retail traffic.** The 20260607 prod snapshot: 61 428 request/response pairs, ~400 of them guild traffic, all with response bodies. | The wire contract — exact field names, nesting, message shapes, page sizes. |
| 3 | **The game's own strings.** `reference/game-defs/loc_strings_en.json` (18 592 keys) and the il2cpp type dump. | The endpoint surface, the enums, and a prose statement of the rules in `UI.Help.Guilds.Description`. |

### On trusting source 1

The asset was not decoded with a typetree generator; it was parsed straight from
the MonoBehaviour's serialized bytes against the field layout in the il2cpp dump.
That sounds fragile, so the check that makes it not fragile: **the reader consumed
13 420 of 13 420 bytes exactly.** A wrong field order or a mis-aligned bool
desyncs by tens of bytes and fails loudly rather than producing plausible
nonsense. Every string field also decoded to a localization key that resolves in
`loc_strings_en.json`.

`server/src/guild_policy.rs` has tests that read `data/guild_data.json` and assert
the constants and the whole matrix still match it, so nobody can quietly edit a
number afterwards.

---

## 2. The model

A guild is up to **20** players. Each user is in **at most one** guild. Guilds
have a name, a 4-digit tag, a banner, a region, two descriptions, a trophy total,
and a lifetime donation counter.

### Guild types — how you get in

il2cpp `enum GuildType`: `OPEN = 0, APPLY_ONLY = 1, CLOSED = 2`. All three appear
on the wire (`OPEN` 98×, `APPLY_ONLY` 76×, `CLOSED` 1× across captured guilds).

| Type | Joining |
|------|---------|
| `OPEN` | **Permissionless.** `POST /guilds/{id}/join` seats you immediately. |
| `APPLY_ONLY` | **Request and approval.** `POST /guilds/{id}/apply` queues you; the Grand Master approves or denies. |
| `CLOSED` | Nobody may join or apply. |

The game says this itself, in `UI.Help.Guilds.Description`:

> Joining an "Apply Only" guild requires the approval of the guild's creator, or
> Grand Master, while joining an Open guild does not. The Grand Master also has
> the power to set the guild to Closed (to prevent new applicants) and to remove
> any members from the guild.

### Ranks — and the thing worth knowing

il2cpp `enum GuildRank`: `GRANDMASTER = 0, MASTER = 1, ELDER = 2, MEMBER = 3`.
Lower number, more authority.

**Only the Grand Master holds any power.** `MASTER` and `ELDER` are cosmetic
titles that retail shipped and never wired up. This is not a simplification we
made — it is what `GuildData._guildRanksData` literally contains, and all three
sources say so:

- The asset grants every flag to `GRANDMASTER` and **none** to the other three,
  across `canEditGuild`, `canApproveGuildApplications`, `canBanNonMembers`, and
  every row of their per-target kick/ban tables.
- The help text (above) attributes every power to the Grand Master and mentions
  no other role.
- Across **1422 captured member records**, exactly two ranks were ever assigned:
  `GRANDMASTER` (76) and `MEMBER` (1346). Not one `MASTER`. Not one `ELDER`.

Consistently, there is **no promote and no demote** anywhere: the client has no
request class for either, and `GuildRankData::CanPromote` / `CanDemote` are
8-byte stubs where the sibling `CanKick` / `CanBan` are 0x128 bytes of real
logic. This server likewise exposes no endpoint that changes a rank.

We keep the four-rank vocabulary anyway, because the wire, the enum, and the UI
strings all carry it, and because a future capture could show otherwise.

### The permission matrix

| Rank | Edit guild | Approve/deny | Kick | Ban |
|------|-----------|--------------|------|-----|
| `GRANDMASTER` | yes | yes | yes | yes |
| `MASTER` | no | no | no | no |
| `ELDER` | no | no | no | no |
| `MEMBER` | no | no | no | no |

Kick and ban additionally require **strictly outranking** the target. The asset
encodes that explicitly — `GRANDMASTER` has `canKick = true` against MASTER,
ELDER and MEMBER, and `canKick = false` against GRANDMASTER. So:

- Nobody can kick or ban a Grand Master.
- Nobody can remove somebody of their own rank.
- A plain member can do nothing to anybody.

### Removals and the re-join cooldown

Leaving, being kicked, and being banned all write a `guild_removals` row.

- **Kick / leave** — blocks re-joining *that* guild for **7 days**
  (`GuildData._admissionTimeoutAfterRemovalFromGuildInSeconds` = 604 800).
  Surfaces as `UI.Guild.Error.JoinTimeout.Body`: *"You have been removed from
  this guild. You may try to join again in {0}."*
- **Ban** — never expires.
- Being **approved** back into the guild clears the record.

### Succession

If the Grand Master leaves, the most senior surviving member inherits — rank
first, then earliest join date — and a `PROMOTE` entry is posted. If nobody is
left, the guild is deleted along with its board and pending applications.

This is **modelled**, not observed (see §5).

### The message board

`guild_messages` is a typed log. `type` values are retail's `GuildMessageType`:
`CLIENT`, `JOIN`, `APPROVE`, `DENY`, `KICK`, `BAN`, `LEAVE`, `PROMOTE`,
`DONATE`, `GUILD_UPDATE`.

Message ids are `{creationTime}::{uuid}`, e.g.
`1778851566::ee7662d4-ab1d-41e9-ab52-b24ef5b8762f`. Newest first. 30 per page
(`GuildData._messageBoard._maxMessagesToDisplay`).

`typeSpecificData` payloads, from 1531 captured entries:

| type | payload | seen |
|------|---------|------|
| `CLIENT` | `{type, text}`, plus `unfilteredText` when the profanity filter changed something | 903 |
| `DONATE` | `{type, requesterUserId, requesterCharacterId, itemTemplateId, donatedAmount}` | 534 |
| `JOIN` | `{}` | 33 |
| `LEAVE` | `{}` | 14 |
| `GUILD_UPDATE` | `{type, …only the changed fields}` | 13 |
| `KICK` | `{type, kickedUserId}` | 9 |
| `APPROVE` | `{type, approvedUserId}` | 4 |
| `DENY` | `{type, deniedUserId}` | 1 |
| `BAN`, `PROMOTE` | — never captured, see §5 | 0 |

In every case the **actor** is the message's own `userId`/`characterId`, and the
target (if any) is inside `typeSpecificData`.

Retail ran chat through a six-language profanity filter, storing the cleaned copy
as `text` and the original as `unfilteredText` (94 of 903 messages — i.e. only
when filtering changed something). **We have no profanity list**, so `text` is
always the player's own words and `unfilteredText` is correctly never emitted.
Length bounds (1–500 characters) *are* enforced.

---

## 3. Endpoints

Paths are il2cpp's, from the `URL_PATH` constants on the request classes in
`BGS.Shared.Rest.Api.BladeServer`. All are under
`/api/game/v1/public/characters/{characterId}`.

| Method | Path | Who | Response |
|--------|------|-----|----------|
| GET | `/guilds/current` | member | `{guild, members}` |
| POST/PUT | `/guilds/current` | **GM** | `{guild, members}` |
| GET | `/guilds/current/messages` | member | `{guildMessageBoard}` or `{}` |
| POST | `/guilds/current/messages` | member | `{guildMessageBoard}` |
| GET | `/guilds/current/applications` | **GM** | `{guildApplications}` |
| POST | `/guilds/current/approve/{userId}` | **GM** | `{member}` |
| POST | `/guilds/current/deny/{userId}` | **GM** | `{}` |
| POST | `/guilds/current/kick/{userId}` | **GM** | `{}` |
| POST | `/guilds/current/ban/{userId}` | **GM** | `{}` |
| POST | `/guilds/current/leave` | member | `{}` |
| GET | `/guilds/search` | any | `{guilds}` |
| GET | `/guilds/leaderboard` | any | `{guildLeaderboard, playerGuildLeaderboardEntry}` |
| POST | `/guilds` | guildless | `{guild, members}` |
| POST | `/guilds/{id}/join` | guildless | `{member}` |
| POST | `/guilds/{id}/apply` | guildless | `{guildApplication}` |
| GET | `/guilds/{id}` | any | `{applicationStatus, guild, members}` |

Plus the pre-existing exchange endpoints under `/guilds/current/exchanges`.

### Wire details worth not re-deriving

- **A guild object** carries `id, name, tagId, type, shortDescription,
  longDescription, badgeIconIndex, regionIndex, memberCount,
  guildExchangeDonationCount, pvpTrophies, grandmasterSinceSecs`. Note
  `pvpTrophies`, not `trophies`.
- Retail also sent `pvpSeasonId`. **We omit it**: il2cpp `GuildInfo` has no such
  property, so the client parses and discards it. Emitting a season id we cannot
  make meaningful would be inventing data nobody reads.
- **`applicationStatus`** is `{maxApplicationsReached: bool}` and sits at the
  *response* level of `GET /guilds/{id}`, not inside the guild object.
- **The leaderboard** is `{currentPage, totalPages, entries}` where each entry is
  a guild object with a flat `rank` key. 100 entries per page, 1-based — every
  captured `page=1` carried exactly 100, ranked 1..100.
- **Empty message windows return the literal `{}`**, not
  `{"guildMessageBoard": []}`. 23 captured polls confirm it.
- **Message paging** takes `oldestCreationTime` (exclusive lower bound — "what's
  new since?") and `newestCreationTime` (exclusive upper bound — "let me scroll
  back"). The client polls with a steadily increasing `oldestCreationTime`.

### Join refusals

`JoinRefusal` mirrors il2cpp `CanJoinGuildResult` one-for-one, and the error code
is retail's own ordinal offset by 100:

| Code | Meaning |
|------|---------|
| 101 | already in a guild |
| 102 | already applied somewhere |
| 103 | guild is closed |
| 104 | guild is full |
| 105 | removed too recently / banned |
| 106 | too many pending applications |
| 107 | no such guild |
| 200 | below `minLevelToJoin` — **ours**, deliberately outside retail's range |

Refusals are reported in that precedence order, so a caller who trips several
conditions hears the one retail's client would have pre-checked.

---

## 4. Tunables

All four from `GuildData`, all cross-checked.

| Constant | Value | Corroboration |
|----------|-------|---------------|
| `MAX_MEMBERS` | 20 | 175 captured guilds, `memberCount` never exceeds 20; help text says "groups of up to 20 players" |
| `MAX_APPLICATIONS` | 10 | client's default search sends `applicationCountMax=9`, one below the cap — exactly as its `memberCountMax=19` sits one below 20 |
| `MIN_LEVEL_TO_JOIN` | 5 | — |
| `REJOIN_COOLDOWN_SECS` | 604 800 (7 days) | `UI.Guild.Error.JoinTimeout.Body` formats a duration |
| `MESSAGE_PAGE_LIMIT` | 30 | `_messageBoard._maxMessagesToDisplay` |
| chat message length | 1–500 | — |
| guild name / short / long | 3–40 / 1–200 / 0–5000 | — |
| `LEADERBOARD_PAGE_SIZE` | 100 | capture only (not in the asset) |

---

## 5. What is modelled, and what a human should check

Everything above is recovered retail behaviour **except** the following. Each is
labelled at its definition in the source, and each has a test pinning it so that
changing it is deliberate.

1. **Grand Master succession** (`guild_policy::successor`). Retail's behaviour is
   unrecoverable — no capture contains a Grand Master leaving, and there is no
   promotion request to observe. But something must happen: since the Grand
   Master is the *only* holder of every power, a guild that lost theirs with no
   successor would be permanently frozen. We promote the most senior survivor.
2. **A full `APPLY_ONLY` guild still accepts applications.** Joining a full guild
   is refused; *applying* to one is allowed, and the fullness check moves to
   approval time. Rationale: retail's search treats member count and application
   count as independent filters, and refusing applications to every popular guild
   would make `APPLY_ONLY` guilds unjoinable in practice, since popular guilds sit
   at 20.
3. **A removal blocks applying as well as joining.** The UI string does not
   distinguish the two paths, and a cooldown walkable-around by applying instead
   would not be a cooldown.
4. **`BAN` and `PROMOTE` payload key names.** Never captured. `bannedUserId` and
   `promotedUserId` follow the `kickedUserId` / `approvedUserId` / `deniedUserId`
   convention, which holds for all three observed cases.
5. **Application response key names** — `guildApplication`, `guildApplications`,
   and the `applicationState` field. From il2cpp property names
   (`ResponseGuildApplicationsData._guildApplications`, `GuildApplication._userId`
   etc.), camelCased like every other field on this API. No capture exists,
   because no captured player ever applied to an `APPLY_ONLY` guild.
6. **The HTTP verb for guild edit.** il2cpp `UpdateGuildRequest.URL_PATH` is
   `/characters/{0}/guilds/current` — the same path `GetCurrentGuildRequest`
   GETs — and no update crossed the wire. **Both POST and PUT are registered**
   against the same handler, so whichever the client uses is served.
7. **`GUILD_UPDATE` keys for banner and region.** Only `guildType`,
   `shortDescription` and `longDescription` were ever observed. We use
   `badgeIconIndex` and `regionIndex`, matching what those fields are called
   everywhere else on the wire.
8. **`LEADERBOARD_PAGE_SIZE = 100`** is capture-derived but not asset-confirmed;
   the asset has no such field, so it may have been a server-side constant.

---

## 6. Known gaps

Deliberately not implemented, in rough priority order.

- **Guild creation is free.** Retail charged **50 Gems**
  (`GuildData._createCosts`, corroborated by the help text). Wiring this needs
  the economy helpers already used by the exchange handlers; it was left out to
  keep this change to guild mechanics.
- **No profanity filter**, so `unfilteredText` is never emitted (see §2). Retail
  shipped a 507-code-point blocklist and six language lists; only the length
  bounds and not the code-point whitelist are enforced here.
- **`pvpTrophies` is never updated.** The column exists and is served, but
  nothing writes it — guild trophy accrual belongs to arena scoring, which is a
  separate workstream.
- **No `INVITED` flow.** `GuildApplicationState` has an `INVITED` member and the
  UI has `UI.Guild.Buttons.NoApplications` = "(No invites available)", but the
  client ships no invite request, so there is nothing to serve.
- **Guild tags are not unique.** Retail's tags look like unique 4-digit ids and
  the search box is "Find by tag or name"; we generate a random 4-digit tag with
  no uniqueness constraint. Collisions are possible at scale (unlikely at ours).
- **`minLevelToJoin` costs an extra query.** Join and apply load the character
  blob to read `level`. Fine at this scale; worth noting if joins ever get hot.

---

## 7. Testing

`server/src/guild_policy.rs` holds 40 unit tests. They are deliberately
weighted toward the **negatives** — a permission model tested only on what it
allows is untested.

Two of them read `data/guild_data.json` directly and assert that every constant
and every cell of the permission matrix still matches the extracted asset. The
boundary tests are written *relative* to the constants, so they would keep
passing if someone edited one; these would not.

### Proving the tests actually fail

Run:

```
python3 script/mutation_test_guild_policy.py
```

It breaks one rule at a time — 25 mutations covering the permission matrix, the
outranking requirement, every join precondition, the cooldown, the ban, the
succession order, the text bounds, and each individual constant — runs the guild
tests, and reports which fail. A mutation that leaves the suite green means that
rule is untested, and the script exits non-zero.

As of this change: **all 25 mutations are caught.**
