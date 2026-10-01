#!/usr/bin/env python3
"""Regenerate `blades_lib/src/spawn_group_counts.json` from the APK.

How many enemies each SPAWNER of an enemy spawn group holds (tracker #301).

Every dungeon's `DungeonSettings` ScriptableObject (the `dungeonsettings`
bundle) lists its enemy spawn groups under `_spawnSettings._spawnGroupsEnemy[]`.
Each entry carries:

    _uid._id                 the spawn-group id `enemyGeneratedData` is keyed by
    _quantity                how many spawners (the outer list) -- parsed.json
                             already has this
    _countPerSpawnerMin/Max  how many enemies stand at EACH spawner (the inner
                             list) -- parsed.json dropped both
    _levelDeltaSequence      per-group level offsets (kept for reference; the
                             generator does not read it yet)

The generator put exactly one enemy at every spawner. "The Troll Trap"
(quest 45c042e1) counts kills on group d971b82c only, whose APK min/max is 3/3
and where retail sent three trolls -- so 3/3 was unreachable.

Retail always sent a per-spawner list length within min..max (7,666 distinct
group generations in the capture corpus, 0 outside), and the 13 observed
generations of min-2/max-3 groups all came back 3, so the server uses `max`.

The output is embedded in the server binary (`include_str!`), so a code merge
ships it -- it is NOT a `deploy/static` file.

USAGE -- straight from an extracted APK (needs UnityPy >= 1.25 plus the
TypeTreeGeneratorAPI package: the dungeonsettings bundle has no embedded
typetrees, so they are generated from libil2cpp.so + global-metadata.dat):

    python3 script/extract_spawn_group_counts.py \\
        --apk-extract ~/Projects/blades-re-cache/blades-apk-extract

or from a groups dump already taken from that bundle (gid -> {min, max,
delta, ...}, the shape this script's `read_apk` returns):

    python3 script/extract_spawn_group_counts.py --groups-json groups.json
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
    "spawn_group_counts.json",
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
        for e in settings["_spawnSettings"].get("_spawnGroupsEnemy", []):
            gid = uid(e)
            if not gid or gid == "0":
                continue
            out[gid] = {
                "dungeon": uid(settings),
                "handle": tt.get("m_Name"),
                "quantity": e.get("_quantity"),
                "min": e.get("_countPerSpawnerMin"),
                "max": e.get("_countPerSpawnerMax"),
                "delta": e.get("_levelDeltaSequence"),
            }
    return out


def build(groups):
    entries = {}
    for gid, g in groups.items():
        lo, hi = int(g["min"]), int(g["max"])
        if lo < 1 or hi < lo:
            sys.exit(f"{gid}: impossible per-spawner count {lo}..{hi}")
        entries[gid] = {"min": lo, "max": hi, "levelDeltaSequence": list(g.get("delta") or [])}
    return entries


def write(entries, path):
    meta = {
        "description": "Enemies per spawner for every enemy spawn group, from the APK "
        "dungeonsettings bundle (_countPerSpawnerMin/_countPerSpawnerMax, _levelDeltaSequence).",
        "use": "generate_for_dungeon puts `max` enemies at each spawner; a group absent here "
        "gets 1 (min 1, max 1, no deltas).",
        "evidence": "retail per-spawner list length was within min..max in 7666/7666 distinct "
        "group generations; min2/max3 groups were 3 in 13/13 (tracker #301).",
        "generator": "script/extract_spawn_group_counts.py",
        "groups": len(entries),
        "groupsWithMaxAboveOne": sum(1 for e in entries.values() if e["max"] > 1),
    }
    lines = ["{", ' "_meta": ' + json.dumps(meta, sort_keys=True) + ",", ' "groups": {']
    keys = sorted(entries)
    for i, k in enumerate(keys):
        sep = "," if i + 1 < len(keys) else ""
        lines.append(f"  {json.dumps(k)}: {json.dumps(entries[k], sort_keys=True)}{sep}")
    lines += [" }", "}", ""]
    with open(path, "w") as f:
        f.write("\n".join(lines))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--apk-extract", help="extracted APK root (assets/, lib/)")
    src.add_argument("--groups-json", help="pre-dumped groups (gid -> {min,max,delta})")
    ap.add_argument("--out", default=OUT_DEFAULT)
    args = ap.parse_args()

    if args.apk_extract:
        groups = read_apk(os.path.expanduser(args.apk_extract))
    else:
        with open(args.groups_json) as f:
            groups = json.load(f)
    entries = build(groups)
    write(entries, args.out)
    print(
        f"{len(entries)} groups, {sum(1 for e in entries.values() if e['max'] > 1)} with max > 1"
        f" -> {os.path.normpath(args.out)}"
    )


if __name__ == "__main__":
    main()
