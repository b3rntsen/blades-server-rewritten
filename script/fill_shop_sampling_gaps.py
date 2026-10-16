#!/usr/bin/env python3
"""Close the sampling gap in a merchant's measured potion tail.

Why this exists
---------------
Report #397 (HauDrauf): "The Alchemist's offer doesn't seem random: the same
tier-9 potions always show up and others never."

`extract_shop_level_stock.py` builds each level's pool from the bundles retail
LISTED, weighted by how often it listed them. That is right for bundles retail
listed often, and wrong for a rare tail. A level-9 Alchemist catalog holds
Health T8 (108/108 catalogs), about six ingredients, Magicka T8 (19/108),
Stamina T8 (12/108) and, now and then, one other potion or poison of tier 7-9.
Those other potions came up 46 times in 108 catalogs, spread 1-3 times over
the 26 kinds that came up at all. 40 kinds sit in that tier window (14 per
tier, minus the three restoration T8s above), so a uniform draw expects about
12 kinds never to show in 108 catalogs. Twelve never did, including five of
the fourteen tier-9 kinds. The extractor read "never seen" as "never sold",
so our Alchemist could not offer those twelve at all and favoured the others.

What this does
--------------
For each Alchemist level with at least `MIN_CATALOGS` retail catalogs (only
level 9 has them; levels 0-8 hold 1-11), inside the tier window retail's
potions span at that level:

  * the ANCHOR (listed in >= 90% of catalogs: Health T8 at level 9) is left
    alone;
  * FREQUENT potions (listed in >= 5%: Magicka T8, Stamina T8) are left alone;
  * the rest are one RARE class. Every APK potion or poison of a tier in the
    window that the shop sells (a single-stack bundle in shop_bundles.json)
    joins that class, and the class's measured total weight is shared equally
    among its members.

The total weight of the class stays the same, so a catalog still holds as many
potions as retail's did; only which ones is spread evenly. Bundle prices and
`maxItems` are untouched. Every bundle the pool already held stays in it.

Added entries carry `_gapFill`; re-weighted ones keep retail's fitted weight
in `_retailWeight`. The script is idempotent: it first undoes its own edits,
so it can run after every re-measure (extract_shop_level_stock.py calls it).

  python3 script/fill_shop_sampling_gaps.py --stock deploy/static/shop_stock.json --write
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys

ALCHEMIST = "e1dd10fc-8b14-4288-9b23-99b0d58388de"
MIN_CATALOGS = 30
ANCHOR_SHARE = 0.9
FREQUENT_SHARE = 0.05
POTION = re.compile(r"^Items\.Name\.(?:Potion|Poison)\..*\.Tier(\d+)$")
LISTED = re.compile(r"^(\d+)/(\d+)$")


def potion_bundles(bundles: dict, names: dict) -> dict:
    """bundleId -> potion tier, for every bundle that sells one potion stack."""
    out = {}
    for bid, b in bundles.items():
        grant = b.get("grant") or {}
        stacks = grant.get("stackableItems") or {}
        if grant.get("items") or len(stacks) != 1:
            continue
        m = POTION.match(names.get(next(iter(stacks)), {}).get("name", ""))
        if m:
            out[bid] = int(m.group(1))
    return out


def undo(pool: list) -> list:
    kept = []
    for e in pool:
        if e.get("_gapFill"):
            continue
        if "_retailWeight" in e:
            e["weight"] = e.pop("_retailWeight")
        kept.append(e)
    return kept


def fill_level(pool: list, potions: dict, level: str) -> tuple[list, str | None]:
    pool = undo(pool)
    listed = {}
    for e in pool:
        m = LISTED.match(str(e.get("_retailListed", "")))
        if m:
            listed[e["bundleId"]] = (int(m.group(1)), int(m.group(2)))
    n = max((t for _, t in listed.values()), default=0)
    if n < MIN_CATALOGS:
        return pool, None
    tiers = [potions[e["bundleId"]] for e in pool if e["bundleId"] in potions]
    if not tiers:
        return pool, None
    lo, hi = min(tiers), max(tiers)
    share = {b: c / t for b, (c, t) in listed.items()}
    rare = [
        e for e in pool
        if e["bundleId"] in potions and share.get(e["bundleId"], 0.0) < FREQUENT_SHARE
    ]
    held = {e["bundleId"] for e in pool}
    missing = sorted(b for b, t in potions.items() if lo <= t <= hi and b not in held)
    if not missing or not rare:
        return pool, None

    # Quantity band per tier: what retail listed for this level's other potions
    # of that tier (the anchor's stack is its own thing).
    bands = {}
    for e in pool:
        b = e["bundleId"]
        if b in potions and share.get(b, 0.0) < ANCHOR_SHARE:
            t = potions[b]
            lo_q, hi_q = bands.get(t, (e["minQuantity"], e["maxQuantity"]))
            bands[t] = (min(lo_q, e["minQuantity"]), max(hi_q, e["maxQuantity"]))

    mass = sum(e["weight"] for e in rare)
    each = max(1, round(mass / (len(rare) + len(missing))))
    for e in rare:
        e["_retailWeight"] = e["weight"]
        e["weight"] = each
    for b in missing:
        q_lo, q_hi = bands.get(potions[b], (1, 1))
        pool.append({
            "bundleId": b,
            "weight": each,
            "minQuantity": q_lo,
            "maxQuantity": q_hi,
            "_retailListed": f"0/{n}",
            "_gapFill": (
                f"report #397: a tier-{potions[b]} potion the APK sells; retail's level-{level} "
                f"rare potions (tiers {lo}-{hi}) came up about once per 100 catalogs each, so "
                f"{n} catalogs miss about a third of them by chance. Shares the rare class's weight."
            ),
        })
    note = (
        f"report #397: {len(missing)} unlisted tier {lo}-{hi} potions added; the rare potion class "
        f"({len(rare)} listed + {len(missing)} added) shares retail's measured weight {mass} "
        f"at {each} each (script/fill_shop_sampling_gaps.py)"
    )
    return pool, note


def fill(stock: dict, bundles: dict, names: dict) -> list[str]:
    gen = stock["generation"][ALCHEMIST]
    # Only the Alchemist's own goods (its authored pool holds every potion bundle).
    own = {e["bundleId"] for e in gen["itemPool"]}
    potions = {b: t for b, t in potion_bundles(bundles, names).items() if b in own}
    report = []
    for level, pool in sorted(gen.get("levelPools", {}).items(), key=lambda kv: int(kv[0])):
        gen["levelPools"][level], note = fill_level(pool, potions, level)
        params = gen["levels"][level]
        params.pop("_potionTailFill", None)
        if note:
            params["_potionTailFill"] = note
            report.append(f"Alchemist L{level}: {note}")
    return report


def load(path: str):
    with open(path) as f:
        return json.load(f)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--stock", default="deploy/static/shop_stock.json")
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args()
    static = os.path.dirname(args.stock)
    stock = load(args.stock)
    report = fill(
        stock,
        load(os.path.join(static, "shop_bundles.json")),
        load(os.path.join(static, "parsed.json"))["items_template"],
    )
    print("\n".join(report) or "nothing to fill")
    if args.write:
        with open(args.stock, "w") as f:
            json.dump(stock, f, indent=2)
            f.write("\n")


if __name__ == "__main__":
    sys.exit(main())
