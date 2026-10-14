#!/usr/bin/env python3
"""Derive per-tier chest loot tables from captured retail chest openings.

Retail rolled a chest's contents server-side at open time, and the loot tables
themselves were never shipped in the APK (only referenced).  What we *do* have is
752 retail-era captures of `POST /characters/{uuid}/chests/{slot}/collect`, each
carrying the exact `reward` Bethesda's server rolled.  This script turns those
into evidence-based per-tier pools.

The wire never names the chest's tier -- the URL carries only the treasury *slot*
id ("1", "2", ... per character).  Tiers are recovered by replaying each
character's capture stream in capture-id order and tracking its treasury:

  * `GET /character/{uuid}/inventories/current` -> `inventory.treasury.chests[]`
    is a FULL listing ({id, tier, level}); it replaces the tracked map.
  * quest completions, abyss updates and other reward endpoints carry a partial
    `inventory.treasury.chests[]` listing only the chests just ADDED; they upsert.
  * `inventory.treasury.removedChests[]` (what a collect itself returns) deletes.

At a collect we look the slot id up in the map as of that moment.

Two phases so the expensive half runs once, on the capture box:

    # on the capture box (read-only; bodies live in the archive DB)
    sudo nice -n 15 python3 extract_chest_loot.py opens \
        --db /var/lib/newblades/db/blades.db \
        --archive /var/lib/newblades/db/blades-archive.db \
        --out /tmp/chest_opens.jsonl

    # anywhere, from the intermediate
    python3 script/extract_chest_loot.py tables \
        --opens chest_opens.jsonl \
        --out deploy/static/chest_loots.json \
        --stats chest_loot_stats.json

Both databases are opened read-only (`mode=ro` + `PRAGMA query_only`); this
script never writes to them.
"""

from __future__ import annotations

import argparse
import gzip
import json
import re
import sqlite3
import sys
from collections import Counter, defaultdict
from pathlib import Path

# Everything after this timestamp is our own re-implementation answering, not
# Bethesda.  Retail shut down 2026-06-30.
RETAIL_CUTOFF = "2026-06-30"

COLLECT_RE = re.compile(
    r"/characters/(?P<character>[0-9a-fA-F-]{36})/chests/(?P<slot>[^/?]+)/collect"
)
UUID_RE = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")

# A minimum sample count below which we refuse to publish a tier's table: with a
# handful of observations we cannot tell a table from an accident.
MIN_SAMPLES = 30

# The gold currency template id, used only for the per-level-band gold summary.
GOLD = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2"


# --------------------------------------------------------------------------- #
# phase 1: opens
# --------------------------------------------------------------------------- #


def open_ro(path: str) -> sqlite3.Connection:
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    conn.execute("PRAGMA query_only=1")
    return conn


class BodyReader:
    """Response bodies live inline in blades.db for old rows and in the archive
    DB for everything the 'DB diet' moved out.  Read both, newest layout first."""

    def __init__(self, main: sqlite3.Connection, archive: sqlite3.Connection | None):
        self.main = main
        self.archive = archive
        self.hits_main = 0
        self.hits_archive = 0
        self.misses = 0

    def __call__(self, capture_id: int) -> bytes:
        row = self.main.execute(
            "SELECT response_body FROM api_captures WHERE id=?", (capture_id,)
        ).fetchone()
        blob = row[0] if row and row[0] else None
        if blob:
            self.hits_main += 1
        elif self.archive is not None:
            row = self.archive.execute(
                "SELECT response_body FROM capture_bodies WHERE capture_id=?", (capture_id,)
            ).fetchone()
            blob = row[0] if row and row[0] else None
            if blob:
                self.hits_archive += 1
        if not blob:
            self.misses += 1
            return b""
        if isinstance(blob, str):
            blob = blob.encode()
        if blob[:2] == b"\x1f\x8b":
            blob = gzip.decompress(blob)
        return blob


def find_treasuries(node, path=""):
    """Yield (json-path, value) for every `treasury` / `overflowTreasury` object."""
    if isinstance(node, dict):
        for key, value in node.items():
            if key in ("treasury", "overflowTreasury") and isinstance(value, dict):
                yield (f"{path}/{key}", value)
            yield from find_treasuries(value, f"{path}/{key}")
    elif isinstance(node, list):
        for item in node:
            yield from find_treasuries(item, f"{path}[]")


def phase_opens(args) -> int:
    main = open_ro(args.db)
    archive = open_ro(args.archive) if args.archive else None
    body = BodyReader(main, archive)

    collects = main.execute(
        "SELECT id, user_id, timestamp, url, response_status FROM api_captures "
        "WHERE url LIKE '%/chests/%/collect%' ORDER BY id"
    ).fetchall()
    characters = set()
    collect_ids = {}
    for cid, _user, _ts, url, _status in collects:
        m = COLLECT_RE.search(url)
        if m:
            characters.add(m.group("character").lower())
            collect_ids[cid] = m
    print(f"collect captures: {len(collects)}  characters: {len(characters)}", file=sys.stderr)

    # Candidate captures that could mutate or state a treasury: anything issued
    # for one of those characters.  Loopback (127.0.0.1:808x) is our own server
    # answering, not Bethesda, so it is excluded from tier resolution entirely.
    placeholders = ",".join("?" * len(characters))
    rows = main.execute(
        "SELECT id, user_id, timestamp, url, method, response_status FROM api_captures "
        "WHERE url NOT LIKE 'http://127.0.0.1%' "
        "  AND url NOT LIKE '%/analytics/%' "
        "  AND url NOT LIKE '%announcements.blades%' "
        "  AND (url LIKE '%/character/%' OR url LIKE '%/characters/%') "
        "ORDER BY id"
    ).fetchall()

    def character_of(url: str) -> str | None:
        m = re.search(r"/characters?/([0-9a-fA-F-]{36})", url)
        return m.group(1).lower() if m else None

    candidates = [r for r in rows if (character_of(r[3]) in characters)]
    print(f"candidate captures to scan: {len(candidates)}", file=sys.stderr)

    # per character: slot id -> {"tier":, "level":, "from":capture_id, "source":url-kind}
    treasury: dict[str, dict[str, dict]] = defaultdict(dict)
    source_counts = Counter()
    unresolved_slots = Counter()
    ever_seen: dict[str, set] = defaultdict(set)
    out = Path(args.out).open("w")
    written = 0
    unresolved = 0
    scanned = 0

    for cid, user_id, ts, url, method, status in candidates:
        scanned += 1
        if scanned % 2000 == 0:
            print(f"  ... {scanned}/{len(candidates)}", file=sys.stderr)
        char = character_of(url)
        collect = collect_ids.get(cid)

        raw = body(cid)
        payload = None
        if raw and (b'"chests"' in raw or b'"removedChests"' in raw or collect):
            try:
                payload = json.loads(raw)
            except Exception:
                payload = None

        # Record the open BEFORE applying this response's own removedChests.
        if collect:
            slot = collect.group("slot")
            known = treasury[char].get(slot)
            reward = (payload or {}).get("reward")
            rec = {
                "capture_id": cid,
                "timestamp": ts,
                "user_id": user_id,
                "character_id": char,
                "slot": slot,
                "fast_track": "fastTrack=True" in url,
                "response_status": status,
                "retail": ts < RETAIL_CUTOFF,
                "tier": known["tier"] if known else None,
                "level": known["level"] if known else None,
                "tier_from_capture": known["from"] if known else None,
                "tier_source": known["source"] if known else None,
                "reward": reward,
            }
            if known is None:
                unresolved += 1
                unresolved_slots[(char, slot)] += 1
            out.write(json.dumps(rec, sort_keys=True) + "\n")
            written += 1

        if payload is None:
            continue

        # A GET of the whole inventory is authoritative: it replaces the map.
        full_listing = "/inventories/current" in url and method == "GET"
        for _path, node in find_treasuries(payload):
            chests = node.get("chests")
            if isinstance(chests, list) and chests:
                kind = "full" if full_listing else "delta"
                if full_listing:
                    treasury[char] = {}
                for chest in chests:
                    if not isinstance(chest, dict) or "id" not in chest:
                        continue
                    ever_seen[char].add(str(chest["id"]))
                    treasury[char][str(chest["id"])] = {
                        "tier": chest.get("tier"),
                        "level": chest.get("level"),
                        "from": cid,
                        "source": kind,
                    }
                source_counts[(kind, UUID_RE.sub("{u}", url.split("?")[0]))] += len(chests)
            for removed in node.get("removedChests") or []:
                treasury[char].pop(str(removed), None)

    out.close()
    print(f"\nwrote {written} opens ({unresolved} with an unresolved tier) to {args.out}", file=sys.stderr)
    print(f"bodies: inline={body.hits_main} archive={body.hits_archive} missing={body.misses}", file=sys.stderr)
    never = sum(n for (ch, slot), n in unresolved_slots.items() if slot not in ever_seen[ch])
    print(f"unresolved opens whose slot was NEVER seen for that character: {never}", file=sys.stderr)
    print(f"unresolved opens whose slot WAS seen at some point (timing/eviction): {unresolved - never}", file=sys.stderr)
    print("\ntier evidence by endpoint (chest entries seen):", file=sys.stderr)
    for (kind, pattern), n in source_counts.most_common():
        print(f"  {n:6d}  {kind:5s}  {pattern}", file=sys.stderr)
    return 0


# --------------------------------------------------------------------------- #
# phase 2: tables
# --------------------------------------------------------------------------- #


def normalise_reward(reward: dict) -> dict:
    """A captured reward with its keys ordered so identical bundles compare equal.

    The instanced item `id` is KEPT: `RewardItem.id` is a required field, and the
    collect handler re-mints it per grant anyway (a captured id would otherwise
    collide across players)."""
    out = {}
    if reward.get("currencies"):
        out["currencies"] = dict(sorted(reward["currencies"].items()))
    if reward.get("stackableItems"):
        out["stackableItems"] = dict(sorted(reward["stackableItems"].items()))
    items = []
    for item in reward.get("items") or []:
        copy = dict(item)
        props = copy.get("properties") or {}
        if props.get("ENCHANTING") or props.get("GRADING"):
            # Keep GRADING too (#392 audit): rebuilding properties from ENCHANTING
            # alone stripped every chest ring's and necklace's ability bonuses.
            copy["properties"] = {
                kind: [dict(sorted(e.items())) for e in props[kind]]
                for kind in ("ENCHANTING", "GRADING")
                if props.get(kind)
            }
        items.append(copy)
    if items:
        out["items"] = items
    return out


def span(values: list) -> dict:
    return {
        "n": len(values),
        "min": min(values),
        "max": max(values),
        "mean": round(sum(values) / len(values), 2),
    }


def tier_summary(sample: list, rewards: list) -> dict:
    currencies = defaultdict(list)
    stackables = defaultdict(list)
    templates = Counter()
    ench_tiers = Counter()
    ench_per_item = Counter()
    tempering = Counter()
    item_counts = Counter()
    for r in rewards:
        for cur, amount in (r.get("currencies") or {}).items():
            currencies[cur].append(amount)
        for item, qty in (r.get("stackableItems") or {}).items():
            stackables[item].append(qty)
        items = r.get("items") or []
        item_counts[len(items)] += 1
        for item in items:
            templates[item["itemTemplateId"]] += 1
            ench = (item.get("properties") or {}).get("ENCHANTING") or []
            ench_per_item[len(ench)] += 1
            tempering[item.get("temperingLevel")] += 1
            for e in ench:
                if e.get("tier") is not None:
                    ench_tiers[e["tier"]] += 1

    levels = [o["level"] for o in sample if o["level"] is not None]
    gold_by_band = defaultdict(list)
    for o, r in zip(sample, rewards):
        band = ((o["level"] or 1) - 1) // 10 * 10 + 1
        gold_by_band[band].append((r.get("currencies") or {}).get(GOLD, 0))

    return {
        "samples": len(sample),
        "distinct_bundles": len({json.dumps(r, sort_keys=True) for r in rewards}),
        "distinct_characters": len({o["character_id"] for o in sample}),
        "distinct_users": len({o["user_id"] for o in sample}),
        "chest_level_range": [min(levels), max(levels)] if levels else None,
        "distinct_chest_levels": len(set(levels)),
        "largest_level_gap": max(
            [b - a for a, b in zip(sorted(set(levels)), sorted(set(levels))[1:])] or [0]
        ),
        "instanced_items_per_open": dict(sorted(item_counts.items())),
        "currencies": {c: span(v) for c, v in sorted(currencies.items())},
        "gold_by_level_band": {
            f"{b}-{b + 9}": span(v) for b, v in sorted(gold_by_band.items())
        },
        "enchantments_per_item": dict(sorted(ench_per_item.items())),
        "enchantment_tiers": dict(sorted(ench_tiers.items())),
        "tempering_levels": {str(k): v for k, v in sorted(tempering.items(), key=lambda kv: str(kv[0]))},
        "distinct_item_templates": len(templates),
        "item_templates": {
            t: {"drops": n, "per_open": round(n / len(sample), 4)}
            for t, n in templates.most_common()
        },
        "distinct_stackables": len(stackables),
        "stackables": {
            i: {**span(v), "open_frequency": round(len(v) / len(sample), 4)}
            for i, v in sorted(stackables.items(), key=lambda kv: -len(kv[1]))
        },
    }


def phase_tables(args) -> int:
    opens = [json.loads(line) for line in Path(args.opens).open() if line.strip()]
    stats = {"input_opens": len(opens), "min_samples_to_publish": MIN_SAMPLES}

    kept, dropped, starter = [], Counter(), []
    for o in opens:
        if not o["retail"]:
            dropped["post-retail (our own server answering)"] += 1
        elif o["response_status"] != 200:
            dropped[f"HTTP {o['response_status']} (no reward rolled)"] += 1
        elif not o.get("reward"):
            dropped["200 but no reward block"] += 1
        elif o["tier"] is None:
            dropped["chest tier never observed"] += 1
        elif o["tier"] < 0:
            # Retail's tutorial chest carries tier -1. Our `Chest.tier` is a u64 and
            # cannot even hold it, so it is reported, never published.
            dropped[f"tutorial chest (tier {o['tier']})"] += 1
            starter.append(o)
        else:
            kept.append(o)
    stats["dropped"] = dict(dropped)
    stats["kept"] = len(kept)

    if starter:
        rewards = [normalise_reward(o["reward"]) for o in starter]
        stats["tutorial_chest_tier_minus_1"] = {
            "samples": len(starter),
            "chest_levels": sorted({o["level"] for o in starter}),
            "slots": sorted({o["slot"] for o in starter}),
            "summary": tier_summary(starter, rewards),
        }

    by_tier = defaultdict(list)
    for o in kept:
        by_tier[o["tier"]].append(o)

    tier_stats, published, provisional = {}, {}, {}
    for tier in sorted(by_tier):
        sample = by_tier[tier]
        rewards = [normalise_reward(o["reward"]) for o in sample]
        tier_stats[str(tier)] = tier_summary(sample, rewards)
        pool = [
            {"chestLevel": o["level"], "reward": r}
            for o, r in sorted(
                zip(sample, rewards), key=lambda pair: (pair[0]["level"] or 0, pair[0]["capture_id"])
            )
        ]
        (published if len(sample) >= MIN_SAMPLES else provisional)[str(tier)] = pool

    stats["tiers"] = tier_stats
    stats["published_tiers"] = sorted(published)
    stats["provisional_tiers"] = sorted(provisional)

    document = {
        "_comment": (
            "Per-tier chest loot pools derived from retail capture. Every entry is a "
            "reward bundle Bethesda's server actually returned for a chest of that tier, "
            "verbatim, tagged with the chest level it was rolled at. Nothing here is "
            "synthesised, smoothed or extrapolated. Generated by "
            "script/extract_chest_loot.py -- do not hand-edit."
        ),
        "_source": (
            "blades-capture api_captures + blades-archive capture_bodies, "
            f"retail era only (timestamp < {RETAIL_CUTOFF})"
        ),
        "_method": "docs/chest-loot-extraction.md",
        "_samples": {t: len(v) for t, v in sorted(published.items())},
        "_provisionalSamples": {t: len(v) for t, v in sorted(provisional.items())},
        "tiers": published,
        "provisionalTiers": provisional,
    }
    Path(args.out).write_text(json.dumps(document, indent=2) + "\n")
    if args.stats:
        Path(args.stats).write_text(json.dumps(stats, indent=2, default=str) + "\n")
    print(json.dumps({k: v for k, v in stats.items() if k not in ("tiers", "tutorial_chest_tier_minus_1")}, indent=2, default=str))
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("opens", help="scan the capture DBs and emit one JSON line per chest open")
    p.add_argument("--db", default="/var/lib/newblades/db/blades.db")
    p.add_argument("--archive", default="/var/lib/newblades/db/blades-archive.db")
    p.add_argument("--out", required=True)
    p.set_defaults(func=phase_opens)

    p = sub.add_parser("tables", help="aggregate the opens into deploy/static/chest_loots.json")
    p.add_argument("--opens", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--stats")
    p.set_defaults(func=phase_tables)

    args = ap.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
