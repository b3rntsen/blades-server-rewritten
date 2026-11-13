#!/usr/bin/env python3
"""Rebuild deploy/static/challenges.json from the APK's whole challenge library
(tracker #466).

The pool used to be the 45 templates someone happened to hold in the June 2026
captures. Retail's library (`BaseChallengeData` assets in the APK's
`gameplaymetadata` bundle) has 2,401, and at level 100 the captured pool offered
one damage challenge (Bashing) where retail had Slashing, Cleaving, Bashing,
Fire, Frost, Shock and Poison, each also as a "+" variant, and a "use ability X"
challenge for most abilities. This script writes every template retail could
offer by player level alone:

  * dropped: `_weight` 0 (402 assets, PvP "defeat players" among them; none of
    the captured templates has weight 0, so retail never drew them),
    `_oneTimeOnly` (the tutorial set, #426), and any selector gated on
    something this server does not evaluate (town level, Abyss level, building,
    quest) -- those are the town/build/brew challenges;
  * kept as captured: a template already in the pool keeps its captured quota
    and reward (a real retail instance);
  * new: quota = the middle of `[_minQuantity, _maxQuantity]` rounded to
    `_roundToQuantity` (retail drew it in that range), reward = the
    highest-weight entry of its `ChallengeRewardData` (all gold or gems);
  * annotated as before (#406/#466): level band, `requiredAbilities`,
    `targetEnemyGroup`, plus `category` (the `_challengePoolCategory`: the
    gold "common" category or the gem uncommon/rare/epic/legendary ones) and
    `weight` (`_weight`, retail's draw weight).

NOT applied: the selector's `_requiredDamageType` (e.g. Cleaving challenges need
damage type 2). Retail decided that server-side and the client carries no code
for it, so what it was checked against (equipped weapon? inventory?) is unknown.

Reads the bundle with plain UnityPy (typetrees are embedded in
`gameplaymetadata`; no TypeTreeGeneratorAPI needed):

    unzip -q reference/apk/blades.apk 'assets/Bundles/gameplaymetadata' -d /tmp/apk
    python3 script/build_challenge_pool.py /tmp/apk/assets/Bundles/gameplaymetadata \
        deploy/static/challenges.json
"""
import json
import sys

import UnityPy

GOLD = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2"
GEMS = "470c8f58-a8dd-4c07-8c92-843b785e1139"
NIL = ("", "0")

bundle, path = sys.argv[1], sys.argv[2]

objs = {}
challenges = []
for o in UnityPy.load(bundle).objects:
    if o.type.name != "MonoBehaviour":
        continue
    try:
        tt = o.read_typetree()
        cn = o.read().m_Script.read().m_ClassName
    except Exception:
        continue
    objs[o.path_id] = tt
    if cn.endswith("ChallengeData") and cn != "ChallengeData":
        tt["_class"] = cn
        challenges.append(tt)


def ref(ptr):
    return objs[ptr["m_PathID"]]


def ptr_id(p):
    return ((p or {}).get("_uid") or {}).get("_id", "0")


def gated_elsewhere(sel):
    """A selector condition other than player level / abilities / damage type."""
    ints = ("_requiredTownLevel", "_lockedAtTownLevel", "_requiredAbyssLevel",
            "_lockedAtAbyssLevel", "_requiredBuildingLevel", "_lockedAtBuildingLevel")
    ptrs = ("_requiredQuestPointer", "_lockedAtQuestPointer", "_requiredBuildingTypeId",
            "_requiredBuildingCategoryId")
    return any(sel.get(k) for k in ints) or any(ptr_id(sel.get(k)) not in NIL for k in ptrs)


def quota(c):
    lo, hi, step = c["_minQuantity"], c["_maxQuantity"], c["_roundToQuantity"] or 1
    q = round((lo + hi) / 2 / step) * step
    return float(max(q, lo, 1))


def reward(c):
    items = ref(c["_reward"])["_itemList"]
    best = max(items, key=lambda i: i["_weight"])
    item = best["_itemPointer"]["_uid"]["_id"]
    assert item in (GOLD, GEMS), f"{c['m_Name']}: reward item {item}"
    return {"currencies": {item: int(best["_minQuantity"])}}


# Enemy groups this server's job generator cannot spawn at a challenge's level
# (server/src/quest.rs JOB_FAMILIES): no dragon family at all, and wisps only
# from level 24. A "defeat N" challenge for them could never be finished, so a
# template whose band starts below this level is dropped (HauDrauf, #466: retail
# never handed out a challenge you could not complete).
UNSPAWNABLE_BELOW = {
    "c73831de-58be-4950-80e1-ddfa0dd671a7": 10**9,  # Dragons
    "f0f07752-564d-4e2c-b621-3cf9f9dfaa7e": 24,  # Wisps
}

captured = {t["templateId"]: t for t in json.load(open(path))}
pool = []
for c in challenges:
    sel = ref(c["_selector"])
    tid = c["_uid"]["_id"]
    old = captured.get(tid)
    if c["_oneTimeOnly"] or gated_elsewhere(sel) or (c["_weight"] <= 0 and not old):
        continue
    t = {
        "templateId": tid,
        "name": c["m_Name"],
        "objective": old["objective"] if old else {
            "type": "Backend.DefaultObjective",
            "_uid": {"_id": tid},
            "_location": 1,
            "_quota": quota(c),
        },
        "reward": old["reward"] if old else reward(c),
        "requiredPlayerLevel": int(sel["_requiredPlayerLevel"]),
        "lockedAtPlayerLevel": int(sel["_lockedAtPlayerLevel"]),
        "category": c["_challengePoolCategory"]["_uid"]["_id"],
        "weight": float(c["_weight"]),
    }
    target = ptr_id(c.get("_targetEnemyDataPointer"))
    if c["_class"] == "DefeatChallengeData" and target not in NIL:
        if not old and t["requiredPlayerLevel"] < UNSPAWNABLE_BELOW.get(target, 0):
            continue
        t["targetEnemyGroup"] = target
    required = [ptr_id(r["_abilityPointer"]) for r in sel.get("_requiredAbilities") or []]
    required = [r for r in required if r not in NIL]
    if required:
        t["requiredAbilities"] = required
    pool.append(t)

# The tutorial templates the old pool carried stay (#426 retires held ones by id).
kept = {t["templateId"] for t in pool}
pool += [t for tid, t in captured.items() if tid not in kept]
pool.sort(key=lambda t: (t["name"], t["templateId"]))
with open(path, "w") as f:
    json.dump(pool, f, indent=2, sort_keys=True)
    f.write("\n")
print(f"wrote {len(pool)} templates ({len(captured)} before)", file=sys.stderr)
