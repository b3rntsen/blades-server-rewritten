#!/usr/bin/env python3
"""Regenerate `server/src/building_construction.json` from the APK.

What the town charges and pays for building, straight from the client's own
ScriptableObjects (tracker #287):

  * `gameplaymetadata` / BuildingConstructionDataList — per building type,
    `_districts[0]` (`ConstructionDistrictDetails`):
        _softCurrencyCostConstruction[n]   gold to PLACE the (n+1)-th of its family
        _constructionTimesInSeconds[n]     …and how long that build takes
        _softCurrencyCostUpgrade[L]        gold to upgrade TO level L
        _upgradeTimesInSeconds[L]          …and how long that takes
        _softCurrencyCostDestruction[n]    gold to tear one down while n stand
        _buildingLimit                     how many the town may hold
    plus `_lotSize` and the building category; and per STYLE per LEVEL
    (`_styles[j]._levels[k]`, report #287):
        _requireTownLevel                  town level needed to build / upgrade TO
                                           level k in style j — there is no
                                           per-building requirement, only these
        _prestigeForLevel, _prestigeForStyle   town XP (recorded for comparison;
                                           the server still pays prestige from
                                           building_upgrades.json + style_prestige.json)
  * `common` / TownPointsData — `_pointsRewardedForBuildingSites[]`: per
    district, the town XP paid for the k-th building site ever cleared.

`tools/apk-extract/extract_town_static.py` (blades-capture) produces
`building_upgrades.json` from the same class but never reads `_districts`;
that file keeps materials / requireTownLevel / prestige / styles, and this one
owns gold, time, limits and lot points. It is embedded in the server binary
(`include_str!`), so a code merge ships it — it is NOT a `deploy/static` file.

Retail checks (109 completions, 50 placements, 20 clean destroys) are in the
tests in `server/src/town_construction.rs`.

USAGE — either straight from the bundles (needs UnityPy >= 1.25; these bundles
carry embedded typetrees, no il2cpp generator needed):

    python3 script/extract_building_construction.py \\
        --bundles /tmp/apk-extract-work/apk-bundles/assets/Bundles

or from typetree dumps already taken with UnityPy (`read_typetree()` of the
two MonoBehaviours, written as JSON):

    python3 script/extract_building_construction.py \\
        --construction-json gameplaymetadata_BuildingConstructionDataList.json \\
        --points-json TownPointsData.json
"""
import argparse
import json
import os
import re
import sys

OUT_DEFAULT = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "..", "server", "src", "building_construction.json"
)

# Building categories (`_buildingCategoryUidPtr`) whose buildings are built on a
# building site and so CLEAR it. Retail never clears the lot under a wall, the
# main gate, a ruin or an empty-lot marker (measured: every captured town shows
# `lotsCleared: [false]` under all five walls/gates and `true` under every
# finished shop / house / town hall). Archway and Alcove (decor category
# 32397553…) are never placed in any capture, so they are left out rather than
# guessed.
CATEGORY_SHOP = "80b2c021-3eda-4ee1-983b-e8403defdf3f"
CATEGORY_HOUSE = "280fd6d4-c411-4505-9bed-3f3843fba3b9"
CATEGORY_TOWN_HALL = "d34e334c-b99a-4699-af42-05d3f28d681d"
CLEARS_LOTS = {CATEGORY_SHOP, CATEGORY_HOUSE, CATEGORY_TOWN_HALL}


def uid(p):
    return ((p or {}).get("_uid") or {}).get("_id")


def find_mb(env, class_name):
    for o in list(env.objects):
        if o.type.name != "MonoBehaviour":
            continue
        try:
            if o.read().m_Script.read().m_ClassName == class_name:
                return o.read_typetree()
        except Exception:
            pass
    return None


def load_from_bundles(bundles_dir):
    try:
        import UnityPy
    except ImportError:
        sys.exit("UnityPy not installed. python3 -m venv V && V/bin/pip install 'UnityPy>=1.25'")
    paths = [os.path.join(bundles_dir, b) for b in ("gameplaymetadata", "common", "common_client")]
    env = UnityPy.load(*[p for p in paths if os.path.exists(p)])
    construction = find_mb(env, "BuildingConstructionDataList")
    points = find_mb(env, "TownPointsData")
    if construction is None or points is None:
        sys.exit("BuildingConstructionDataList / TownPointsData not found in the bundles")
    return construction, points


def seconds_to_ms(values, what):
    out = []
    for v in values:
        ms = round(float(v) * 1000)
        if abs(float(v) * 1000 - ms) > 1e-6:
            sys.exit(f"{what}: {v} s is not a whole number of milliseconds")
        out.append(ms)
    return out


def per_style_levels(entry):
    """`{styleId: {editorName, requireTownLevel[k], prestigeForLevel[k],
    prestigeForStyle[k]}}`, k = `_level`. A style may list a level twice (Ruins
    lists level 2 three times); the copies must agree, and the levels must run
    0..n without a gap so an array index IS the level."""
    fields = ("_requireTownLevel", "_prestigeForLevel", "_prestigeForStyle")
    out = {}
    for st in entry.get("_styles") or []:
        sid = uid(st.get("_stylePointer"))
        by_level = {}
        for lv in st.get("_levels") or []:
            row = tuple(int(lv.get(f, 0)) for f in fields)
            prev = by_level.setdefault(lv["_level"], row)
            if prev != row:
                sys.exit(f"{entry.get('_editorName')}/{sid}: level {lv['_level']} listed twice, differently")
        if sorted(by_level) != list(range(len(by_level))):
            sys.exit(f"{entry.get('_editorName')}/{sid}: levels {sorted(by_level)} are not 0..n")
        rows = [by_level[k] for k in range(len(by_level))]
        out[sid] = {
            "editorName": st.get("_editorName"),
            "requireTownLevel": [r[0] for r in rows],
            "prestigeForLevel": [r[1] for r in rows],
            "prestigeForStyle": [r[2] for r in rows],
        }
    return dict(sorted(out.items()))


def build(construction, points):
    buildings = {}
    for e in construction["_templateList"]:
        type_id = uid(e["_typePointer"])
        districts = e.get("_districts") or []
        if not districts:
            continue
        d = districts[0]
        category = uid(e.get("_buildingCategoryUidPtr"))
        buildings[type_id] = {
            "editorName": e.get("_editorName"),
            "lotSize": e.get("_lotSize", 1),
            # Placement price/limit count the whole family: the three house
            # types share one count (a House A placed after ten mixed houses
            # costs construction[10]); every other type counts only itself.
            "family": "house" if category == CATEGORY_HOUSE else type_id,
            "clearsLots": category in CLEARS_LOTS,
            "buildingLimit": d["_buildingLimit"],
            "constructionGold": list(d["_softCurrencyCostConstruction"]),
            "constructionMs": seconds_to_ms(d["_constructionTimesInSeconds"], type_id),
            "upgradeGold": list(d["_softCurrencyCostUpgrade"]),
            "upgradeMs": seconds_to_ms(d["_upgradeTimesInSeconds"], type_id),
            "destructionGold": list(d["_softCurrencyCostDestruction"]),
            "styles": per_style_levels(e),
        }
    lot_points = {
        uid(p["_districtId"]): list(p["_pointsRewarded"])
        for p in points["_pointsRewardedForBuildingSites"]
    }
    return {
        "_meta": {
            "description": "Per-building gold/time/limit/destroy arrays and building-site "
            "town XP, from the APK. Regenerate with script/extract_building_construction.py.",
            "sources": [
                "gameplaymetadata: BuildingConstructionDataList._templateList[]._districts[0]",
                "gameplaymetadata: BuildingConstructionDataList._templateList[]._styles[]._levels[]",
                "common: TownPointsData._pointsRewardedForBuildingSites",
            ],
            "indexing": "constructionGold/constructionMs/destructionGold[n]: n = buildings of "
            "the same family standing in the town (any state) before the action; "
            "upgradeGold/upgradeMs[L]: L = level being upgraded TO; lotPoints[district][k]: "
            "town XP for the k-th site cleared in that district; "
            "styles[styleId].requireTownLevel/prestigeForLevel/prestigeForStyle[k]: k = the level "
            "built or upgraded TO, in that style.",
        },
        "lotPoints": lot_points,
        "buildings": dict(sorted(buildings.items())),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bundles", help="unzipped assets/Bundles dir")
    ap.add_argument("--construction-json", help="typetree dump of BuildingConstructionDataList")
    ap.add_argument("--points-json", help="typetree dump of TownPointsData")
    ap.add_argument("--out", default=OUT_DEFAULT)
    a = ap.parse_args()
    if a.bundles:
        construction, points = load_from_bundles(a.bundles)
    elif a.construction_json and a.points_json:
        construction = json.load(open(a.construction_json))
        points = json.load(open(a.points_json))
    else:
        ap.error("give --bundles, or both --construction-json and --points-json")
    data = build(construction, points)
    text = json.dumps(data, indent=1)
    # One line per number array, so a diff of a regeneration reads per building.
    text = re.sub(
        r"\[\s*(-?\d[\d\s,.-]*?)\s*\]",
        lambda m: "[" + ", ".join(x.strip() for x in m.group(1).split(",")) + "]",
        text,
    )
    with open(a.out, "w") as f:
        f.write(text + "\n")
    print(f"wrote {a.out}: {len(data['buildings'])} building types")


if __name__ == "__main__":
    main()
