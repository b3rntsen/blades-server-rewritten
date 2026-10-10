#!/usr/bin/env python3
"""Emit blades_lib/src/legendary_gear.json: what a Legendary chest's gear slots
are GENERATED from -- the APK's loot templates and enchanting tables.

WHY (#368). The composer used to pick each gear slot from the few dozen pieces
retail happened to pay in the 4,697 recorded purchases. Retail did not pick from
a list; it generated the piece. Measured on the corpus (2,372 regular-slot
items) and checked here on every run:

  * the template is one of the 21 loot templates of the piece's material tier:
    every corpus template is in that pool, and at Silver, Daedric and Dragon
    (the tiers with enough draws) all 21 of the pool appear;
  * the primary enchantment is one of the APK enchant recipes allowed on that
    template (recipe type / slot / weapon-class filters) AT the item's enchant
    tier -- recipes exist only at odd tiers for some families (Fire, Shock,
    Resist Fire...) and only at even ones for the rest (Frost, Poison...);
  * the secondaries are distinct members of the template's APK secondary table
    (weight > 0), at the same tier;
  * durability is the APK max durability at the item's tempering level.

Not one corpus item breaks a rule (the script exits non-zero if one does).

What the APK does NOT say, and is therefore measured: how often each kind of
equipment comes up (`categoryWeights`, regular slots of the corpus) -- retail
paid a weapon about three times as often as a helmet, not 9-12 times as the
per-template count would give. Within a kind every template is equally likely
(the APK lootWeight is the same for every template of a tier), and primary and
secondary draws are uniform (the corpus frequencies are flat; the crafting
weights in enchanting.json are not what the chest used).

usage: extract_legendary_gear.py <items.json> <enchanting.json> <item_durability.json>
                                 <store_bundle_loot.json> <out.json>
"""
import collections
import json
import struct
import sys

GEAR_TYPES = {2: "weapon", 3: "armor", 9: "shield", 10: "ring", 11: "necklace"}
ARTIFACT_TAG = "8511534e-ef06-4e6a-910a-745a2cc72a47"
NPC_EXCLUSIVE_TAG = "3d7877f2-a3be-4d61-ab7d-29beed6b840f"
CANNOT_BE_ENCHANTED_TAG = "30725603-5c67-4d95-bf47-a087744d7a5f"
LEGENDARY_CORPUS = "11102495-fde7-4e77-b6c4-d13b9303f1f5"
# Recipe filterSlots use the equipment slot; a ring is slot 9, a necklace 8.
JEWELRY_SLOT = {10: 9, 11: 8}


def f32(x):
    return struct.unpack("f", struct.pack("f", x))[0]


def shortest(x):
    """The shortest decimal that is the same f32 -- what retail printed."""
    for d in range(0, 10):
        if f32(round(x, d)) == f32(x):
            return round(x, d)
    return x


def category(it):
    name = GEAR_TYPES[it["type"]]
    if name == "armor":
        return f"armor{it['stats']['equipmentSlot']}"
    return name


def recipe_allows(r, it):
    s = it.get("stats", {})
    if r["filterTypes"] and it["type"] not in r["filterTypes"]:
        return False
    if r["filterSlots"]:
        slot = s.get("equipmentSlot") if it["type"] == 3 else JEWELRY_SLOT.get(it["type"])
        if slot not in r["filterSlots"]:
            return False
    if r["filterWeaponClass"] and s.get("weaponClass") not in r["filterWeaponClass"]:
        return False
    if r["filterWeaponType"] and s.get("weaponType") not in r["filterWeaponType"]:
        return False
    return True


def main(items_path, enchanting_path, durability_path, corpus_path, out_path):
    items = json.load(open(items_path))
    ench = json.load(open(enchanting_path))
    dur = json.load(open(durability_path))
    corpus = json.load(open(corpus_path))

    tables = [
        sorted(p["id"] for p in t["properties"] if p["weight"] > 0)
        for t in ench["secondaryTables"]
    ]

    pool = {}
    groups = []          # distinct primary groups: [{property: [tiers]}]
    for uid, it in sorted(items.items()):
        if it.get("type") not in GEAR_TYPES or not it.get("lootWeight"):
            continue
        tags = set(it.get("tags", []))
        if tags & {ARTIFACT_TAG, NPC_EXCLUSIVE_TAG, CANNOT_BE_ENCHANTED_TAG}:
            continue
        if uid not in ench["templates"]:
            continue
        prim = collections.defaultdict(set)
        for r in ench["recipes"].values():
            if recipe_allows(r, it):
                prim[r["property"]].add(r["tier"])
        group = {p: sorted(t) for p, t in sorted(prim.items())}
        if group not in groups:
            groups.append(group)
        jewelry = it["type"] in JEWELRY_SLOT
        if not jewelry and uid not in dur:
            sys.exit(f"{uid} ({it['editor_name']}) has no durability row")
        pool[uid] = {
            "name": it["editor_name"],
            "requiredLevel": it["requiredLevel"],
            "tier": it["tier"],
            "category": category(it),
            "lootWeight": it["lootWeight"],
            "secondaryTable": ench["templates"][uid],
            "primaryGroup": groups.index(group),
            "durability": None if jewelry else [shortest(dur[uid][str(t)]) for t in range(11)],
        }

    # Within a material tier every template carries the same APK lootWeight, so a
    # uniform pick within a kind IS the lootWeight-weighted pick.
    lw = collections.defaultdict(set)
    for e in pool.values():
        lw[(e["requiredLevel"], e["tier"])].add(e["lootWeight"])
    uneven = {k: v for k, v in lw.items() if len(v) != 1}
    if uneven:
        sys.exit(f"lootWeight varies within a tier: {uneven}")

    # ---- the identity test: every retail regular-slot piece obeys the rules ----
    product = next(p for p in corpus["products"] if p["productId"] == LEGENDARY_CORPUS)
    weights = collections.Counter()
    fails = collections.Counter()
    checked = 0
    per_tier = collections.defaultdict(set)
    for band in product["byLevel"]:
        core = min(len(r["reward"]["items"]) for r in band["results"])
        for r in band["results"]:
            for it in r["reward"]["items"][:core]:
                checked += 1
                t = it["itemTemplateId"]
                entry = pool.get(t)
                if entry is None:
                    fails["template not in pool"] += 1
                    continue
                per_tier[(entry["requiredLevel"], entry["tier"])].add(t)
                if not entry["category"] in ("ring", "necklace"):
                    weights[entry["category"]] += 1
                    want = entry["durability"][it.get("temperingLevel", 0)]
                    if abs(want - it["durability"]) > 0.005:
                        fails["durability"] += 1
                ens = it.get("properties", {}).get("ENCHANTING", [])
                if not ens:
                    continue
                tiers = {e["tier"] for e in ens}
                if len(tiers) != 1:
                    fails["mixed enchant tiers"] += 1
                    continue
                tier = tiers.pop()
                group = groups[entry["primaryGroup"]]
                if tier not in group.get(ens[0]["id"], []):
                    fails["primary"] += 1
                secs = [e["id"] for e in ens[1:]]
                if len(set(secs)) != len(secs) or any(
                    s not in tables[entry["secondaryTable"]] for s in secs
                ):
                    fails["secondary"] += 1
    if fails:
        sys.exit(f"{sum(fails.values())} of {checked} retail pieces break a rule: {dict(fails)}")
    print(f"identity: all {checked} retail regular-slot pieces obey the generation rules")
    for key in sorted(per_tier):
        size = sum(1 for e in pool.values() if (e["requiredLevel"], e["tier"]) == key)
        print(f"  requiredLevel {key[0]:2} tier {key[1]:2}: retail paid {len(per_tier[key]):2} of {size} pool templates")

    out = {
        "_meta": {
            "description": "What a Legendary chest's gear slots are generated from (#368): the APK loot "
                           "templates per material tier, their enchanting tables and durability, and the "
                           "measured share of each kind of equipment.",
            "source": "blades-capture reference/game-defs/items.json (APK ItemTemplate lists), "
                      "deploy/static/enchanting.json, deploy/static/item_durability.json, "
                      "blades_lib/src/store_bundle_loot.json (categoryWeights only)",
            "generator": "script/extract_legendary_gear.py",
            "pool": "lootWeight > 0, gear types, not Artifact / NPCExclusive / CannotBeEnchanted",
            "categoryWeights": "regular-slot pieces of the 4,697-purchase Legendary corpus by kind; "
                               "jewellery keeps the kind of the retail piece it replaces because its "
                               "GRADING properties are kind-specific",
        },
        "categoryWeights": dict(sorted(weights.items())),
        "secondaryTables": tables,
        "primaryGroups": groups,
        "templates": pool,
    }
    with open(out_path, "w") as f:
        json.dump(out, f, indent=1)
        f.write("\n")
    print(f"wrote {len(pool)} templates, {len(groups)} primary groups to {out_path}")


if __name__ == "__main__":
    main(*sys.argv[1:6])
