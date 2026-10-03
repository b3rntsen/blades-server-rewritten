#!/usr/bin/env python3
"""Build blades_lib/src/abyss_spawn_group_multipliers.json — the kill-score
multiplier of the enemy each Abyss spawn group spawns (#312).

WHY. The client fills its Abyss reward gauge with
    GetKillScore(enemy.Level - initialPlayerLevel) * enemy.KillScoreMultiplier
and only moves the gauge on when an `/update` response carries `reward`. The
server has to cross each rung on the same kill the client does.
`abyss_kill_multipliers.json` resolves the multiplier per FLOOR and takes the
lowest candidate, so on a floor that mixes a critter with anything else the
server fell behind the client: replaying the one complete retail run in the
capture corpus (character 78f2b668, initialPlayerLevel 10, 61 kills, 9 rungs
paid) through the per-floor table pays only 2 of the 9 rungs on retail's kill.
The kill action names its enemy, though: `enemy_killed` carries
`spawnGroupId`, and the APK's DungeonSettings say which enemy family each
spawn group holds. Resolved per spawn group, the same replay pays all 9 on
retail's kill (11 of 11 captured rung payments across the two runs that paid
any), with no payment retail did not make.

HOW THE CLIENT PICKS THE FAMILY. `SpawnGroupEnemy.SetupEnemyData(level,
canExceedMaxLevel)` (RVA 0x1920AD0) takes `EnemyDataPointer`; if that family
`HasPerfectVariantForLevel` (RVA 0x1960DD0: any band with
`min <= level && (canExceedMaxLevel || level <= max)`, RVA 0x1A34288) it keeps
it, otherwise it walks `_fallbackEnemyData` and takes the first family that
does, and keeps the primary when none does. This builder uses
canExceedMaxLevel = false; the captured runs cannot tell the two apart (both
reproduce 11 of 11), and it only matters above a family's last band.

Usage (straight from an extracted APK, needs UnityPy >= 1.25 + the
TypeTreeGeneratorAPI package, as script/extract_spawn_group_counts.py does):

    python3 script/build_abyss_spawn_group_multipliers.py \\
        --apk-extract ~/Projects/blades-re-cache/blades-apk-extract \\
        --enemies <blades-capture>/reference/game-defs/enemies.json

or from a dump already taken (gid -> {"enemy": uid, "fallback": [uid, ...]}):

    python3 script/build_abyss_spawn_group_multipliers.py --groups-json g.json \\
        --enemies <blades-capture>/reference/game-defs/enemies.json

The output is embedded in the server binary (`include_str!`), so a code merge
ships it; it is not a deploy/static file.
"""
import argparse
import glob
import json
import os
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ABYSS = ROOT / "deploy/static/abyss.json"
PARSED = ROOT / "deploy/static/parsed.json"
OUT = ROOT / "blades_lib/src/abyss_spawn_group_multipliers.json"
# Retail never generated an Abyss floor above difficulty 100 (450 of 450 captured
# slices); the margin keeps a stray level resolvable.
MAX_LEVEL = 150


def pointer(p):
    return (((p or {}).get("_uid") or {}).get("_id")) or "0"


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
            gid = pointer(e)
            if gid == "0":
                continue
            out[gid] = {
                "enemy": pointer(e.get("EnemyDataPointer")),
                "fallback": [pointer(p) for p in e.get("_fallbackEnemyData") or []],
            }
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--apk-extract")
    src.add_argument("--groups-json")
    ap.add_argument("--enemies", required=True, help="reference/game-defs/enemies.json")
    args = ap.parse_args()

    if args.apk_extract:
        groups = read_apk(os.path.expanduser(args.apk_extract))
    else:
        groups = json.loads(Path(args.groups_json).read_text())

    enemies = json.loads(Path(args.enemies).read_text())
    variants = {v.get("guid", k): v for k, v in enemies["variants"].items()}
    families = {f["uuid"]: f for f in enemies["families"].values()}

    def perfect(family, level):
        return any(b["min_level"] <= level <= b["max_level"] for b in family["bands"])

    def variant_multiplier(family, level):
        bands = sorted(family["bands"], key=lambda b: b["min_level"])
        band = next((b for b in bands if b["min_level"] <= level <= b["max_level"]), None)
        if band is None:  # outside every band: the nearest end of the range
            below = [b for b in bands if b["min_level"] <= level]
            band = below[-1] if below else bands[0]
        return variants[band["variant_guid"]]["stats"]["killScoreMultiplier"]

    def resolve(group, level):
        chain = [families.get(group["enemy"])] + [families.get(f) for f in group["fallback"]]
        primary = chain[0]
        chosen = next((f for f in chain if f and perfect(f, level)), primary)
        return chosen["name_internal"], variant_multiplier(chosen, level)

    pool = json.loads(ABYSS.read_text())["dungeonPool"]
    parsed = json.loads(PARSED.read_text())["dungeons"]
    wanted = sorted(
        {
            gid
            for dungeon in pool
            for gid in parsed.get(dungeon, {}).get("spawn_info", {}).get("enemy_spawn_groups", {})
        }
    )
    out, missing = {}, []
    for gid in wanted:
        group = groups.get(gid)
        if not group or families.get(group["enemy"]) is None:
            missing.append(gid)
            continue
        steps, last = [], None
        for level in range(1, MAX_LEVEL + 1):
            name, m = resolve(group, level)
            if (name, m) != last:
                steps.append([level, m, name])
                last = (name, m)
        out[gid] = {"steps": [[lvl, m] for lvl, m, _ in steps], "families": [s[2] for s in steps]}
    if missing:
        raise SystemExit(f"{len(missing)} abyss spawn groups unresolved: {missing[:5]}")

    OUT.write_text(
        json.dumps(
            {
                "_meta": {
                    "description": "Abyss kill-score multiplier per enemy spawn group (#312). "
                    "`steps` is [fromEnemyLevel, multiplier] pairs, a step holding until the "
                    "next; `families[i]` names the family step i spawns.",
                    "rule": "SpawnGroupEnemy.SetupEnemyData: EnemyDataPointer, else the first "
                    "_fallbackEnemyData family with a band containing the level, else the "
                    "primary; the variant is the band containing the level (nearest band "
                    "outside the range); multiplier = variant stats.killScoreMultiplier",
                    "evidence": "replaying retail run 78f2b668 (ipl 10, 61 kills) pays all 9 "
                    "rungs on retail's kill; 11/11 captured rung payments overall",
                    "source": "APK dungeonsettings bundle + reference/game-defs/enemies.json; "
                    "regenerate with script/build_abyss_spawn_group_multipliers.py",
                },
                "spawnGroups": out,
            },
            indent=1,
            sort_keys=True,
        )
        + "\n"
    )
    print(f"wrote {OUT.relative_to(ROOT)}: {len(out)} spawn groups")


if __name__ == "__main__":
    main()
