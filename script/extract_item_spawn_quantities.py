#!/usr/bin/env python3
"""Regenerate `blades_lib/src/item_spawn_quantities.json` from the APK.

How many containers (breakables, floor piles) the client places for each ITEM
spawn group (tracker #353, #358, #365).

Every dungeon's `DungeonSettings` ScriptableObject (the `dungeonsettings`
bundle) lists its item spawn groups under `_spawnSettings._spawnGroupsItem[]`.
Each entry carries `_uid._id` (the key `itemGeneratedData` is keyed by) and
`_quantity`: how many spawn points of that group the client puts in the level.
`parsed.json` dropped `_quantity`.

The server sends one loot result per container. It took that count from
`floor_pile_sizes.json`, which is MINED from retail traffic, and fell back to
ONE for a spawn retail was never captured on. Retail's count is exactly this
`_quantity`: 1,283 of the 1,285 captured spawns agree, and the two that do not
are `JobSpawnGroupsReference` groups the job generator handles separately (the
retail-mined value keeps winning there). Nine event dungeons were never captured
(EQ17, EQ28, EQ30, EQ38-EQ42 and EQ23's unused `_B`), so the Breakable_T1 group
of seven containers got one result and the other six opened empty.

The output is embedded in the server binary (`include_str!`), so a code merge
ships it -- it is NOT a `deploy/static` file.

USAGE -- from an extracted APK (UnityPy >= 1.25 plus TypeTreeGeneratorAPI; the
dungeonsettings bundle has no embedded typetrees, so they are generated from
libil2cpp.so + global-metadata.dat, as `extract_spawn_group_counts.py` does):

    python3 script/extract_item_spawn_quantities.py \\
        --apk-extract ~/Projects/blades-re-cache/blades-apk-extract
"""
import argparse
import glob
import json
import os
import sys

OUT_DEFAULT = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "..",
    "blades_lib",
    "src",
    "item_spawn_quantities.json",
)


def uid(p):
    return ((p or {}).get("_uid") or {}).get("_id")


def read_apk(apk_extract):
    import UnityPy
    from UnityPy.helpers.TypeTreeGenerator import TypeTreeGenerator

    env = UnityPy.load(os.path.join(apk_extract, "assets", "Bundles", "dungeonsettings"))
    objs = list(env.objects)
    gen = TypeTreeGenerator(objs[0].assets_file.unity_version)
    so = glob.glob(os.path.join(apk_extract, "lib", "*", "libil2cpp.so"))[0]
    meta = os.path.join(
        apk_extract, "assets", "bin", "Data", "Managed", "Metadata", "global-metadata.dat"
    )
    with open(so, "rb") as a, open(meta, "rb") as b:
        gen.load_il2cpp(a.read(), b.read())
    env.typetree_generator = gen

    out = {}
    for o in objs:
        if o.type.name != "MonoBehaviour":
            continue
        try:
            tt = o.read_typetree()
        except Exception:
            continue
        settings = tt.get("_settings")
        if not isinstance(settings, dict) or "_spawnSettings" not in settings:
            continue
        for e in settings["_spawnSettings"].get("_spawnGroupsItem", []):
            gid = uid(e)
            if not gid or gid == "0":
                continue
            q = e.get("_quantity")
            if not isinstance(q, int) or q < 0:
                sys.exit(f"{tt.get('m_Name')} {gid}: impossible _quantity {q!r}")
            out[gid] = q
    return out


def write(entries, path):
    meta = {
        "description": "Containers the client places per item spawn group, from the APK "
        "dungeonsettings bundle (_spawnGroupsItem[]._quantity).",
        "use": "floor_pile_size: the retail-mined floor_pile_sizes.json first, then this, "
        "then 1.",
        "evidence": "equals the retail result count on 1283 of 1285 captured spawns; the 2 "
        "others are JobSpawnGroupsReference groups where the retail value still wins.",
        "generator": "script/extract_item_spawn_quantities.py",
        "groups": len(entries),
    }
    lines = ["{", ' "_meta": ' + json.dumps(meta, sort_keys=True) + ",", ' "groups": {']
    keys = sorted(entries)
    for i, k in enumerate(keys):
        sep = "," if i + 1 < len(keys) else ""
        lines.append(f"  {json.dumps(k)}: {entries[k]}{sep}")
    lines += [" }", "}", ""]
    with open(path, "w") as f:
        f.write("\n".join(lines))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--apk-extract", required=True, help="extracted APK root (assets/, lib/)")
    ap.add_argument("--out", default=OUT_DEFAULT)
    args = ap.parse_args()
    entries = read_apk(os.path.expanduser(args.apk_extract))
    if not entries:
        sys.exit("no item spawn groups found -- bundle set or class name changed?")
    write(entries, args.out)
    print(f"{len(entries)} item spawn groups -> {os.path.normpath(args.out)}")


if __name__ == "__main__":
    main()
