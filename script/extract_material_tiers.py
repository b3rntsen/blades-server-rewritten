#!/usr/bin/env python3
"""Emit blades_lib/src/material_tiers.json: the APK tier of every crafting
material, and the lowest level each loot source lists it at (report #398).

WHY. HauDrauf's level curve (#398, 2026-10-16) fades a crafting material out over
the levels after it is introduced: "Later you no longer got Histcarp, Pearls
etc., only things you can actually use." That needs each material's
introduction level.

THE APK says it. Every material template carries a `tier` (1-13) on the same
ladder as gear: tier 3 is Silver (unlocked at level 8), tier 8 Ebony and
Stalhrim (33), tier 10 Dragon (45). `blades_lib::features::level_curve` maps the
tier to its unlock level. Retail's own event ladders (#362,
event_material_ladders.json, 409 observations, none contradicting) step their
materials at exactly those levels.

THE LOOT TABLES are recorded too, as `firstListed`, but NOT used for the
introduction level: their lowest listing is polluted by low-level enemies in
high-level dungeons and single stray rows (Ebony Ingot is listed by a level-8
enemy, Dragon Bone by a level-7 one, Daedra Heart at 4), which would fade Ebony
out at 28, before Ebony gear unlocks at 33. The shop stock is keyed by building
level, not character level (the Forge sells Dwarven ingots at its tier 1), so it
cannot date a material either.

Sources: reference/game-defs/items.json in blades-capture (APK ItemTemplate
lists); the repo's enemy_loot.json, interactable_loot_by_level.json and
deploy/static/chest_loots.json.

usage: extract_material_tiers.py <items.json> <out.json>   (run from the repo root)
"""
import json
import sys


def first_listed():
    out = {}

    def add(src, item, level):
        d = out.setdefault(item, {})
        d[src] = min(d.get(src, 10**9), level)

    enemy = json.load(open("blades_lib/src/enemy_loot.json"))
    for t in enemy["tables"].values():
        for b in t.get("byLevel") or []:
            for r in b["results"]:
                for i in (r["loot"].get("stackableItems") or {}):
                    add("enemy", i, b["minEnemyLevel"])
    floor = json.load(open("blades_lib/src/interactable_loot_by_level.json"))
    for t in floor["tables"].values():
        for b in t.get("byLevel") or []:
            for r in b["results"]:
                for i in (r["loot"].get("stackableItems") or {}):
                    add("container", i, b["minEnemyLevel"])
    chests = json.load(open("deploy/static/chest_loots.json"))
    for samples in list(chests.get("tiers", {}).values()) + list(chests.get("provisionalTiers", {}).values()):
        for s in samples:
            for i in s["reward"].get("stackableItems", {}):
                add("chest", i, s["chestLevel"])
    return out


def main(items_path, out_path):
    items = json.load(open(items_path))
    seen = first_listed()
    materials = {}
    for uid, t in sorted(items.items()):
        if t.get("type_name") != "material" and t.get("type") != "material":
            continue
        entry = {"name": t["name"], "tier": t["tier"]}
        if uid in seen:
            entry["firstListed"] = dict(sorted(seen[uid].items()))
        materials[uid] = entry
    out = {
        "_meta": {
            "description": "APK tier of every crafting material (ItemTemplate.tier, type material), and the lowest level each loot source lists it at. level_curve.rs maps the tier to an introduction level (#398).",
            "source": "blades-capture reference/game-defs/items.json (APK ItemTemplate lists); firstListed from enemy_loot.json, interactable_loot_by_level.json, deploy/static/chest_loots.json",
            "generator": "script/extract_material_tiers.py",
        },
        "materials": materials,
    }
    with open(out_path, "w") as f:
        json.dump(out, f, indent=1, sort_keys=False)
        f.write("\n")
    print(f"{len(materials)} materials -> {out_path}")


if __name__ == "__main__":
    main(*sys.argv[1:])
