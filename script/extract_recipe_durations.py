#!/usr/bin/env python3
"""Extract every craft recipe's retail duration from the APK into
blades_lib/src/recipe_durations.json.

Why this exists
---------------
Tracker report #423: the Forge's recipe screen showed a short craft time, and the
job the server started took five hours. The client draws that screen from its own
`Recipe._duration`; the server never read it. A Smithing recipe it could not find in
the 34 captured recipes fell through to `smith_craftables.json`, whose per-item
durations are AUTHORED (`_approx_duration`): 132 of its 134 items carry 17,999,872 ms,
so a 10-minute Steel Gauntlets became a 5-hour job. Alchemy and decoration recipes
outside the capture had no duration at all and finished instantly.

The durations ship inside the APK. From the il2cpp dump:

    Recipe              (dump.cs:552299)  float _duration      seconds, per craft
                                          int   _maxBatchSize
    TemperingRecipe     _levels[]         {_duration, _inputs} one row per target level
    ItemModifierRecipe  (enchanting)      _duration            seconds

`RecipeData._recipeMap` pairs each crafting type with the list holding its recipes, so
walking it yields every recipe the client knows, keyed by the uid the client sends as
`recipeId`.

Retail check (every deduplicated POST /crafts in the capture corpus, 2026-06-07 snapshot
+ prod archive): `completedAt - Date` equals `_duration * batchSize` for all 231
Smithing / Alchemy / DecorationCrafting / Enchanting starts (11 Alchemy at batch 4-5),
and `_levels[temperingLevel - 1]._duration` for all 13 Tempering starts.

Salvaging (1 s) and Repairing (0 s) are left out: neither starts a timed job here.

Reproduce
---------
  unzip reference/apk/blades.apk \\
      'assets/Bundles/*' 'lib/arm64-v8a/libil2cpp.so' \\
      'assets/bin/Data/Managed/Metadata/global-metadata.dat' -d /tmp/blades-apk-extract
  pip install 'UnityPy==1.25.0' 'TypeTreeGeneratorAPI==0.0.10'
  APK_EXTRACT=/tmp/blades-apk-extract python3 script/extract_recipe_durations.py \\
      --out blades_lib/src

APK: reference/apk/blades.apk in the blades-capture repo, Unity 2019.4.37f1, il2cpp
metadata v24, SHA-256 fd6e55f561542cac41a016975318bc69759e512d5155b3c8945367883c833417.
Same extraction method as `script/extract_enchanting_data.py`.
"""

import argparse
import json
import os
import sys

APK_EXTRACT = os.environ.get("APK_EXTRACT", "/tmp/blades-apk-extract")
BUNDLES = APK_EXTRACT + "/assets/Bundles"
SO = APK_EXTRACT + "/lib/arm64-v8a/libil2cpp.so"
META = APK_EXTRACT + "/assets/bin/Data/Managed/Metadata/global-metadata.dat"
APK_SHA256 = "fd6e55f561542cac41a016975318bc69759e512d5155b3c8945367883c833417"

# Crafting types whose recipes start a timed job (RecipeData._recipeMap editor names).
FLAT = ("Smithing", "Alchemy", "DecorationCrafting", "Enchanting")
LEVELLED = ("Tempering",)


def load():
    import UnityPy
    from UnityPy.helpers.TypeTreeGenerator import TypeTreeGenerator

    for p in (BUNDLES, SO, META):
        if not os.path.exists(p):
            sys.exit("missing %s — see the module docstring for the unzip command" % p)
    env = UnityPy.load(BUNDLES)
    objs = list(env.objects)
    gen = TypeTreeGenerator(objs[0].assets_file.unity_version)
    gen.load_il2cpp(open(SO, "rb").read(), open(META, "rb").read())
    env.typetree_generator = gen
    by_pid = {o.path_id: o for o in objs}
    recipe_data = []
    for o in objs:
        if o.type.name != "MonoBehaviour":
            continue
        try:
            if o.read().m_Script.read().m_ClassName == "RecipeData":
                recipe_data.append(o.read_typetree())
        except Exception:
            continue
    return recipe_data, by_pid


def uid(d):
    inner = d.get("_uid", d) if isinstance(d, dict) else None
    i = inner.get("_id") if isinstance(inner, dict) else None
    return None if i in (None, "", "0") else i


def seconds(v):
    f = float(v)
    if f != int(f) or f < 0:
        sys.exit("non-integral or negative duration %r" % v)
    return int(f)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="blades_lib/src")
    args = ap.parse_args()
    recipe_data, by_pid = load()
    if len(recipe_data) != 1:
        sys.exit("expected exactly one RecipeData, found %d" % len(recipe_data))

    recipes, counts = {}, {}
    for mapping in recipe_data[0].get("_recipeMap") or []:
        label = mapping.get("_editorName")
        if label not in FLAT + LEVELLED:
            continue
        ref = mapping.get("_recipeList") or {}
        lst = by_pid.get(ref.get("m_PathID"))
        if lst is None:
            sys.exit("recipe list for %s did not resolve" % label)
        for r in lst.read_typetree().get("_recipes") or []:
            rid = uid(r)
            if not rid:
                continue
            if label in LEVELLED:
                levels = [seconds(l["_duration"]) for l in r.get("_levels") or []]
                if not levels:
                    sys.exit("tempering recipe %s has no _levels" % rid)
                row = {"craftingType": label, "levelSeconds": levels}
            else:
                row = {"craftingType": label, "seconds": seconds(r["_duration"])}
            if rid in recipes and recipes[rid] != row:
                sys.exit("recipe %s listed twice with different durations" % rid)
            recipes[rid] = row
            counts[label] = counts.get(label, 0) + 1

    out = {
        "_meta": {
            "description": "Recipe id -> retail craft duration in seconds. Flat recipes: "
            "seconds per craft, multiplied by batchSize. Tempering: levelSeconds[temperingLevel - 1].",
            "source": "reference/apk/blades.apk -> assets/Bundles RecipeData._recipeMap "
            "(Recipe._duration, TemperingRecipe._levels[]._duration, ItemModifierRecipe._duration)",
            "apkSha256": APK_SHA256,
            "producedBy": "script/extract_recipe_durations.py",
            "recipeCounts": dict(sorted(counts.items())),
        },
        "recipes": dict(sorted(recipes.items())),
    }
    path = os.path.join(args.out, "recipe_durations.json")
    with open(path, "w") as f:
        f.write("{\n \"_meta\": %s,\n \"recipes\": {\n" % json.dumps(out["_meta"]))
        rows = ["  %s: %s" % (json.dumps(k), json.dumps(v, separators=(",", ":")))
                for k, v in out["recipes"].items()]
        f.write(",\n".join(rows))
        f.write("\n }\n}\n")
    print("wrote %s: %s" % (path, out["_meta"]["recipeCounts"]))


if __name__ == "__main__":
    main()
