#!/usr/bin/env python3
"""Stamp each challenge template in deploy/static/challenges.json with retail's
player-level band and enemy target (report #406).

Retail never offered a challenge outside its `ChallengeSelector`'s player-level
band: `Challenge_DefeatSkeever_4` is `ChallengeSelector_PlayerLevel_15-19`
(`_requiredPlayerLevel` 15, `_lockedAtPlayerLevel` 20). The pool is
capture-derived and carried no band, so every character was offered every
template in rotation, Skeever hunts at level 74 included.

Source: the APK's challenge library (`BaseChallengeData` subclasses and the
`ChallengeSelector` each points at), read with the blades-capture extractor
loader (`reference/game-defs/extract/lib.py`; see its README for the venv and
the unzipped APK). Run from anywhere:

    APK_EXTRACT=/path/to/blades-apk-extract \
      /path/to/ttvenv/bin/python script/annotate_challenge_levels.py \
      /path/to/blades-capture/reference/game-defs/extract deploy/static/challenges.json

Writes, per template: `name` (the asset name, for reviewers), `requiredPlayerLevel`
and `lockedAtPlayerLevel` (0 = no bound, as in the asset), and `targetEnemyGroup`
for a `DefeatChallengeData` that names an enemy group. Fails if a template has
no asset in the APK.
"""
import json
import sys

lib_dir, path = sys.argv[1], sys.argv[2]
sys.path.insert(0, lib_dir)
import lib  # noqa: E402

_, objs = lib.load()
by_uid = {}
for o in objs:
    if o.type.name != "MonoBehaviour":
        continue
    try:
        cn = o.read().m_Script.read().m_ClassName
    except Exception:
        continue
    if not cn.endswith("ChallengeData") or cn == "ChallengeData":
        continue
    tt = o.read_typetree()
    tt["_class"] = cn
    by_uid[tt["_uid"]["_id"]] = tt

pool = json.load(open(path))
for t in pool:
    c = by_uid[t["templateId"]]  # KeyError = a template retail's APK doesn't know
    sel = lib.resolve(c["_selector"]) or {}
    t["name"] = c["m_Name"]
    t["requiredPlayerLevel"] = int(sel.get("_requiredPlayerLevel", 0))
    t["lockedAtPlayerLevel"] = int(sel.get("_lockedAtPlayerLevel", 0))
    target = (c.get("_targetEnemyDataPointer") or {}).get("_uid", {}).get("_id", "0")
    if c["_class"] == "DefeatChallengeData" and target != "0":
        t["targetEnemyGroup"] = target
    else:
        t.pop("targetEnemyGroup", None)
with open(path, "w") as f:
    json.dump(pool, f, indent=2, sort_keys=True)
    f.write("\n")
print(f"annotated {len(pool)} templates", file=sys.stderr)
