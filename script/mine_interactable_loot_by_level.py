#!/usr/bin/env python3
"""Mine retail floor-loot results BY ENEMY LEVEL -> interactable_loot_by_level.json.

WHY
---
`interactable_loot.json` holds every whole result retail rolled for each
floor-item loot table, POOLED over every level. `roll_loot_table` drew from it
level-blind, so a level-2 tutorial pile could roll anything retail ever put in
that table at level 80. Tracker #8: the first story quest (5ad30483) hands every
new player an Ebony Ingot. In retail, across the snapshot this script reads:

  * table 7ee92f4b (the common "materials" table) produced an Ebony Ingot 0 times
    in 954 draws at enemy levels 1-10, and first at level 42;
  * no floor table produced one below level 31.

The bulk of these tables is level-flat (lumber and iron at every level), but the
tail is gated by level (Ebony from 31, e80bee76 from 46). A median-split drift
statistic like the one mine-enemy-loot.py uses cannot see a gate that moves under
1% of a table, so tables are keyed by level whenever they hold enough
observations to fill two bands. Banding a level-flat table costs only sampling
resolution; pooling a gated one hands level-80 loot to a level-2 player.

SHAPE
-----
The same shape `enemy_loot.json` uses for its level-keyed tables, so the Rust
side draws both through one function:

    tables[tableId] = {observations, levelKeyed: true,
                       byLevel: [{minEnemyLevel, maxEnemyLevel, observations,
                                  results: [{loot: {currencies, stackableItems}, n}]}]}

A table too thin to fill two bands is left out, and `roll_loot_table` keeps
drawing it from the pooled `interactable_loot.json`, exactly as before.

LEVEL
-----
A floor pile carries no level of its own. The level is the enemy level of the
SAME generated dungeon (the mode of its enemyGeneratedData[].enemyLevel), which
is the number our generator is handed too. A generation with no enemies is
skipped.

DEDUPLICATION
-------------
The same generated dungeon is re-served in many responses. Deduplicated on
(questId, sha1 of the canonical dungeon entry), as mine-enemy-loot.py does.

RUN
---
    python3 script/mine_interactable_loot_by_level.py \
        --db ~/blades-prod-backups/20260607-112415/blades-snapshot-20260607-112415.db \
        --out blades_lib/src/interactable_loot_by_level.json

Opens the database read-only. Only https://blades.bgs.services/ is mined.
"""
import argparse, collections, hashlib, json, sqlite3, sys

RETAIL_HOST = "https://blades.bgs.services/"
EBONY_INGOT = "75112030-b248-49b0-9c70-0da8dea150d1"


def canon(o):
    return json.dumps(o, sort_keys=True, separators=(",", ":"))


def js(blob):
    if isinstance(blob, bytes):
        try:
            blob = blob.decode()
        except UnicodeDecodeError:
            return None
    try:
        return json.loads(blob)
    except (TypeError, ValueError):
        return None


def generations(o):
    """Every generated-dungeon entry in a response, found structurally."""
    if isinstance(o, dict):
        if "itemGeneratedData" in o and "enemyGeneratedData" in o:
            yield o
            return
        for v in o.values():
            yield from generations(v)
    elif isinstance(o, list):
        for v in o:
            yield from generations(v)


def enemy_level(dg):
    levels = collections.Counter()

    def walk(o):
        if isinstance(o, dict):
            if "enemyLevel" in o:
                levels[o["enemyLevel"]] += 1
                return
            for v in o.values():
                walk(v)
        elif isinstance(o, list):
            for v in o:
                walk(v)

    walk(dg.get("enemyGeneratedData") or {})
    return levels.most_common(1)[0][0] if levels else None


def normalise(loot):
    loot = loot or {}
    return {
        "currencies": loot.get("currencies") or {},
        "stackableItems": loot.get("stackableItems") or {},
    }


def band_levels(by_level, min_band):
    """Greedy contiguous level bands of at least min_band observations each; the
    last band absorbs a short remainder rather than shipping it alone."""
    bands, cur, n = [], [], 0
    for lv in sorted(by_level):
        cur.append(lv)
        n += sum(by_level[lv].values())
        if n >= min_band:
            bands.append(cur)
            cur, n = [], 0
    if cur:
        if bands:
            bands[-1].extend(cur)
        else:
            bands.append(cur)
    return bands


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--min-band", type=int, default=300,
                    help="minimum observations in one level band")
    a = ap.parse_args()

    con = sqlite3.connect(f"file:{a.db}?mode=ro", uri=True)
    seen = set()
    tables = collections.defaultdict(lambda: collections.defaultdict(collections.Counter))
    responses = entries = kept = no_level = 0
    for (rb,) in con.execute(
        "SELECT response_body FROM api_captures WHERE url LIKE ? "
        "AND response_body LIKE '%itemGeneratedData%' ORDER BY id",
        (RETAIL_HOST + "%",),
    ):
        d = js(rb)
        if d is None:
            continue
        responses += 1
        for dg in generations(d):
            entries += 1
            key = (dg.get("questId"), hashlib.sha1(canon(dg).encode()).hexdigest())
            if key in seen:
                continue
            seen.add(key)
            lvl = enemy_level(dg)
            if lvl is None:
                no_level += 1
                continue
            kept += 1
            for pile in (dg.get("itemGeneratedData") or {}).values():
                for result in pile or []:
                    for tid, loot in ((result or {}).get("lootTableLoot") or {}).items():
                        tables[tid][lvl][canon(normalise(loot))] += 1

    out_tables, left_out = {}, {}
    for tid, by_level in sorted(tables.items()):
        total = sum(sum(c.values()) for c in by_level.values())
        if total < 2 * a.min_band:
            left_out[tid] = total
            continue
        bands = []
        for levels in band_levels(by_level, a.min_band):
            merged = collections.Counter()
            for lv in levels:
                merged.update(by_level[lv])
            bands.append({
                "minEnemyLevel": min(levels),
                "maxEnemyLevel": max(levels),
                "observations": sum(merged.values()),
                "results": [{"loot": json.loads(r), "n": n}
                            for r, n in sorted(merged.items(), key=lambda x: (-x[1], x[0]))],
            })
        out_tables[tid] = {"observations": total, "levelKeyed": True, "byLevel": bands}

    # The control that motivated this file: where does Ebony first appear?
    ebony_floor = {}
    for tid, by_level in tables.items():
        lv = [l for l, c in by_level.items() if any(EBONY_INGOT in r for r in c)]
        if lv:
            ebony_floor[tid] = min(lv)

    meta = {
        "description": "Whole floor-loot RESULTS observed in retail, per loot table, "
                       "banded by the enemy level of the same generated dungeon.",
        "authoritative": "every result appeared verbatim in a retail response from "
                         + RETAIL_HOST + "; nothing is interpolated",
        "howToDraw": "pick the byLevel band whose [minEnemyLevel, maxEnemyLevel] "
                     "contains the dungeon's enemy level, or the nearest band if none "
                     "does, and draw one whole result weighted by n. A table absent "
                     "here is drawn from the pooled interactable_loot.json.",
        "generator": "script/mine_interactable_loot_by_level.py",
        "source": a.db.rsplit("/", 1)[-1],
        "minBandObservations": a.min_band,
        "responsesRead": responses,
        "dungeonEntriesSeen": entries,
        "distinctGenerationsKept": kept,
        "generationsWithoutEnemyLevel": no_level,
        "tablesKept": len(out_tables),
        "tablesLeftOutAsTooThin": left_out,
        "ebonyIngotLowestEnemyLevelByTable": dict(sorted(ebony_floor.items())),
    }
    with open(a.out, "w") as f:
        json.dump({"_meta": meta, "tables": out_tables}, f, separators=(",", ":"), sort_keys=True)
        f.write("\n")
    print(json.dumps(meta, indent=1), file=sys.stderr)


if __name__ == "__main__":
    main()
