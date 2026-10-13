#!/usr/bin/env python3
"""Mine retail ABYSS corpse loot -> blades_lib/src/abyss_enemy_loot.json (report #381).

WHY
---
Abyss floors were generated like dungeon floors, and their spawn groups were
never in `enemy_group_tables.json` (that corpus is mined from quest
`dungeonGeneratedDataList`, and the Abyss sends `abyssDungeonGeneratedData`).
Every abyss enemy therefore fell back to the general enemy table 871c2e9b and
dropped its level-banded materials: Pearl and Garlic on shallow floors,
Quicksilver as the only ingot at enemy level 40-44, Nightshade at 46-50.

Retail did not. Every enemy family the APK names "... Family Abyss" rolls table
2d366ee0 (some add 18bafcb5 or 50cd3882), never 871c2e9b
(reference/game-defs/enemies.json), and in the captures 2d366ee0 paid GOLD and
nothing else.

SHAPE
-----
The shapes `enemy_group_tables.json` and `enemy_loot.json` use, so the Rust side
draws through the same functions:

    groups[spawnGroupId] = {sets: [{tables: [...], n}]}
    pooledSets           = [{tables: [...], n}]   # every abyss enemy, for unseen groups
    tables[tableId]      = {levelKeyed, results | byLevel[{minEnemyLevel,
                            maxEnemyLevel, results: [{loot, n}]}]}

DEDUPLICATION
-------------
The pcap re-ingest stored many responses more than once. A response body is
counted once (sha1 of the body).

USAGE
-----
    sudo python3 mine_abyss_enemy_loot.py \
        /var/lib/newblades/db/blades.db /var/lib/newblades/db/blades-archive.db > out.json

Both databases are opened read-only. Only retail traffic (before 2026-06-30) is read.
"""
import collections
import hashlib
import json
import sqlite3
import sys

GOLD = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2"
GOLD_TABLE = "2d366ee0-8087-4d1d-8161-64a7b3e14f93"
# The general enemy table. A few abyss floors use non-abyss families that roll it;
# enemy_loot.json already models it by level from far more observations, so it is
# kept in the table SETS but not re-measured here.
GENERAL_TABLE = "871c2e9b-7e7a-4564-a022-e435dfb8a436"


def main(main_db: str, archive_db: str) -> None:
    m = sqlite3.connect(f"file:{main_db}?mode=ro", uri=True)
    ids = [
        r[0]
        for r in m.execute(
            "select id from api_captures where url like "
            "'https://blades.bgs.services/api/game/v1/public/characters/%/abysses/current/%' "
            "and timestamp < '2026-06-30' and response_status = 200"
        )
    ]
    a = sqlite3.connect(f"file:{archive_db}?mode=ro", uri=True)

    seen = set()
    groups = collections.defaultdict(collections.Counter)
    pooled = collections.Counter()
    results = collections.defaultdict(lambda: collections.defaultdict(collections.Counter))
    floors = 0
    for capture_id in ids:
        row = a.execute(
            "select response_body from capture_bodies where capture_id = ?", (capture_id,)
        ).fetchone()
        if not row or not row[0]:
            continue
        body = row[0] if isinstance(row[0], bytes) else row[0].encode()
        if b"abyssDungeonGeneratedData" not in body:
            continue
        digest = hashlib.sha1(body).hexdigest()
        if digest in seen:
            continue
        seen.add(digest)
        generated = json.loads(body).get("abyssDungeonGeneratedData") or {}
        floors += 1
        for group, spawners in (generated.get("enemyGeneratedData") or {}).items():
            for spawner in spawners:
                for enemy in spawner:
                    loot = enemy.get("lootTableLoot") or {}
                    tables = tuple(sorted(loot))
                    groups[group][tables] += 1
                    pooled[tables] += 1
                    level = enemy.get("enemyLevel")
                    for table, result in loot.items():
                        kept = {
                            k: result[k]
                            for k in ("currencies", "stackableItems", "items")
                            if result.get(k)
                        }
                        results[table][level][json.dumps(kept, sort_keys=True)] += 1

    def sets(counter):
        return [{"tables": list(t), "n": n} for t, n in counter.most_common()]

    def as_results(counter):
        return [{"loot": json.loads(k), "n": n} for k, n in counter.most_common()]

    out_tables = {}
    for table, by_level in results.items():
        if table == GENERAL_TABLE:
            continue
        if table == GOLD_TABLE:
            # Gold scales with enemy level (4-6 at 1, ~200 at 100): one band per
            # observed level, drawn from the nearest band like enemy_loot.json.
            out_tables[table] = {
                "observations": sum(sum(c.values()) for c in by_level.values()),
                "levelKeyed": True,
                "results": None,
                "byLevel": [
                    {"minEnemyLevel": lv, "maxEnemyLevel": lv, "results": as_results(c)}
                    for lv, c in sorted(by_level.items())
                ],
            }
        else:
            # Too thin to key by level: pooled, empty results included.
            pooled_results = collections.Counter()
            for c in by_level.values():
                pooled_results.update(c)
            out_tables[table] = {
                "observations": sum(pooled_results.values()),
                "levelKeyed": False,
                "results": as_results(pooled_results),
                "byLevel": None,
            }

    json.dump(
        {
            "_meta": {
                "description": "Retail ABYSS corpse loot: which tables each abyss spawn group "
                "rolled, and what the abyss-only tables paid. Generated by "
                "script/mine_abyss_enemy_loot.py (report #381).",
                "source": "prod capture DB api_captures, abysses/current/* responses before "
                "2026-06-30, abyssDungeonGeneratedData.enemyGeneratedData[].lootTableLoot, "
                "deduplicated by response body",
                "floors": floors,
                "howToDraw": "look the spawn group up in groups (else pooledSets), draw one "
                "SET weighted by n, then roll each table: from `tables` here, else from "
                "enemy_loot.json (871c2e9b is the general enemy table some abyss floors use).",
            },
            "groups": {g: {"sets": sets(c)} for g, c in sorted(groups.items())},
            "pooledSets": sets(pooled),
            "tables": out_tables,
        },
        sys.stdout,
        indent=1,
        sort_keys=True,
    )


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
