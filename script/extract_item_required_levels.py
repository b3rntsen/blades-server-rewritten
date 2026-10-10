#!/usr/bin/env python3
"""Emit blades_lib/src/item_required_levels.json: the APK's `requiredLevel` for
every item template the store-chest corpus (store_bundle_loot.json) pays.

WHY (#339/#354/#366). A Legendary chest pays two gear slots, and retail chose
each slot's material tier from the buyer's level: slot 1 the tier unlocked at
that level, slot 0 the tier below (or the same tier once the buyer is a few
levels past the unlock). The corpus groups buyers into wide level bands, so the
composer needs each part's tier to pick parts at the buyer's own level rather
than anywhere in the band. `requiredLevel` IS that tier's unlock level (1, 8, 13,
18, 23, 28, 33, 39, 45) -- the number the gear UI shows.

Source: reference/game-defs/items.json in blades-capture (extracted from the APK
ItemTemplate lists by reference/game-defs/extract/x_items.py). Nothing authored.

usage: extract_item_required_levels.py <items.json> <store_bundle_loot.json> <out.json>
"""
import json, sys


def main(items_path, corpus_path, out_path):
    items = json.load(open(items_path))
    corpus = json.load(open(corpus_path))
    paid = {
        i["itemTemplateId"]
        for p in corpus["products"]
        for b in p["byLevel"]
        for r in b["results"]
        for i in r["reward"].get("items", [])
    }
    missing = sorted(t for t in paid if t not in items or "requiredLevel" not in items[t])
    if missing:
        sys.exit(f"{len(missing)} paid templates have no APK requiredLevel: {missing[:5]}")
    out = {
        "_meta": {
            "description": "APK ItemTemplate.requiredLevel for every item template store_bundle_loot.json pays.",
            "source": "blades-capture reference/game-defs/items.json (APK ItemTemplate lists)",
            "generator": "script/extract_item_required_levels.py",
        },
        "requiredLevel": {t: items[t]["requiredLevel"] for t in sorted(paid)},
    }
    with open(out_path, "w") as f:
        json.dump(out, f, indent=1, sort_keys=False)
        f.write("\n")
    print(f"wrote {len(paid)} templates to {out_path}")


if __name__ == "__main__":
    main(*sys.argv[1:4])
