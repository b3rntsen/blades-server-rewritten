# Export and import a character

Moving a character between servers — your local one and production, or two local
ones. Both halves are already in the codebase; this is the connecting tissue,
written down because working it out from the schema takes longer than it should.

## Export

`scripts/export-character.sql` emits **exactly** the payload
`POST /api/dev/v1/import-character` accepts, so nothing has to be reshaped in
between.

```bash
psql -U blades -d blades -t -A -v cid='<character-uuid>' \
  < scripts/export-character.sql > character.json
```

## Import

```bash
curl -X POST http://<host>/blades.bgs.services/api/dev/v1/import-character \
  -H 'Content-Type: application/json' \
  -H "X-Import-Token: $ARENA_IMPORT_TOKEN" \
  --data-binary @character.json
```

The token is the `ARENA_IMPORT_TOKEN` the server was started with. The header is
`X-Import-Token`; a bare token in `Authorization` silently 401s, which has cost
us an afternoon before.

## Four things that will bite you

Each of these bit while the command was being checked, which is why they are
here rather than in someone's memory.

**1. Scope by `id`, never by name.** Names are not unique. On the live server
there are three characters called `mrsiri` and two called `Arwald of Søvngård`;
matching on `character->>'name'` returns several concatenated JSON documents,
which look like valid JSON right up until they abruptly are not.

**2. `-t -A` are not optional.** Without them psql pads the output with
alignment whitespace and appends a row count, and the result does not parse.

**3. The quests live in their own table and are not optional.** `quests.info` is
the quest body and `quests.id` is the `questId` the client keys on. An import
without them arrives with an empty quest table, `get_quests` returns
`quests: []`, and the in-game quest map has nothing to draw — that was tracker
report #58. Job rows are deliberately **not** exported: they are re-rolled
server-side at every reset window.

**4. `userId` travels inside the payload.** The importer creates the `users` row
if it does not exist and never overwrites an existing one, so importing onto a
fresh server works without preparing anything.

## What comes across

The eight columns of `characters` plus the quest tables: `character`, `data`,
`inventory`, `wallet`, `town`, the `quests[]` array and the matching
`dungeonGeneratedDataList`. Verified against a live level-61 character — 8
top-level keys, 17 quests, 17 dungeon bodies, 137 inventory items.

## A trap that is NOT in the export, but is next door

If you are hand-editing a save to inject an item, take the template id from
`reference/game-defs/items.json` in the capture repo, **not** from
`uuid_labels.json`. That file is 14,957 name-string "seed" ids against 1,113
real item ids, and the seeds carry the item's name — so searching it by name and
taking the first hit gives you something that looks like an item id, is not one,
and makes the save unloadable because the client cannot resolve the template.
`Items.Name.SteelLongsword` alone has six such ids (tracker #180).
