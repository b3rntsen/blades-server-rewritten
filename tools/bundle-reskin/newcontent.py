#!/usr/bin/env python3
"""Apply a Newblades content definition (``newcontent/<id>.json``) to stock bundles.

A definition is the whole client-side footprint of one piece of NON-retail
content, written as edits that each name the stock value they replace
(docs/newblades-content-system.md). This script applies them and refuses when a
stock value is not what the definition says, so a definition written against
one build cannot silently land on another.

Three edit kinds, each in the bundle that owns the data:

* ``enemySwaps``  -> ``dungeonsettings`` (built-in; ships in the APK and is
  delivered over the air once its hash differs). Repoints one
  ``SpawnGroupEnemy.EnemyDataPointer``. The enemy TYPE of a dungeon spawn is
  decided here, client-side; the server's ``enemyGeneratedData`` only carries
  level, XP and loot per spawn-group id, so the server needs no change for a
  swap inside a dungeon it already serves.
* ``enemyBands``  -> ``enemies_sab``. Widens an ``EnemyData`` family's variant
  level band (``_variants[-1]._maxLevel``).
* ``loc``         -> ``localization``. Sets a key in every locale; ``{retail}``
  is replaced by that locale's stock text, so ``"Newblades: {retail}"`` prefixes
  the existing name everywhere. A key that does not exist yet is appended.

Each output is read back and every edit re-checked before success is reported.

    python newcontent.py newcontent/nb-draugr-proof.json \\
        --dungeonsettings <apk>/assets/Bundles/dungeonsettings \\
        --enemies <mirror>/enemies_sab --localization <mirror>/localization \\
        --out out-nc/

Then pack with ``build_pack_v2.py`` (see the doc for the exact line; the
``dungeonsettings`` record must be pinned, ``locate`` refuses it).

Needs UnityPy + TypeTreeGeneratorAPI and the APK's il2cpp pair (``APK_EXTRACT``
= a directory holding ``lib/arm64-v8a/libil2cpp.so`` and
``assets/bin/Data/Managed/Metadata/global-metadata.dat``).
"""
import argparse
import json
import os
import sys

import UnityPy
from UnityPy.helpers.TypeTreeGenerator import TypeTreeGenerator

APK = os.environ.get("APK_EXTRACT", "/tmp/blades-apk-extract")
_GEN = {}


def _load(path):
    env = UnityPy.load(path)
    objs = list(env.objects)
    if not objs:
        raise SystemExit("%s holds no objects — wrong file?" % path)
    # One generator per process: the native loader refuses a second il2cpp load,
    # and every bundle of this build shares one il2cpp pair anyway.
    if "g" not in _GEN:
        g = TypeTreeGenerator(objs[0].assets_file.unity_version)
        with open(os.path.join(APK, "lib/arm64-v8a/libil2cpp.so"), "rb") as so, \
             open(os.path.join(APK, "assets/bin/Data/Managed/Metadata/global-metadata.dat"), "rb") as md:
            g.load_il2cpp(so.read(), md.read())
        _GEN["g"] = g
    env.typetree_generator = _GEN["g"]
    return env


def _monos(env):
    for o in env.objects:
        if o.type.name != "MonoBehaviour":
            continue
        try:
            yield o, o.read_typetree()
        except Exception:
            continue


def _save(env, dst):
    data = env.file.save(packer="original")
    with open(dst, "wb") as fh:
        fh.write(data)
    return len(data)


def _dungeon(tree):
    s = tree.get("_settings")
    return s if isinstance(s, dict) and "_spawnSettings" in s else None


def swap_enemies(src, dst, definition):
    swaps = definition.get("enemySwaps", [])
    if not swaps:
        return None
    want_uid = definition["dungeon"]["uid"]
    env = _load(src)
    hit = [(o, t) for o, t in _monos(env)
           if _dungeon(t) and _dungeon(t)["_uid"]["_id"] == want_uid]
    if len(hit) != 1:
        raise SystemExit("dungeon %s found %d times in %s" % (want_uid, len(hit), src))
    o, t = hit[0]
    groups = {g["_uid"]["_id"]: g for g in _dungeon(t)["_spawnSettings"]["_spawnGroupsEnemy"]}
    for s in swaps:
        g = groups.get(s["spawnGroupId"])
        if g is None:
            raise SystemExit("spawn group %s is not in %s" % (s["spawnGroupId"], t["m_Name"]))
        cur = g["EnemyDataPointer"]["_uid"]["_id"]
        if cur == s["to"]:
            continue
        if cur != s["from"]:
            raise SystemExit("group %s points at %s, definition expects %s — refusing"
                             % (s["spawnGroupId"], cur, s["from"]))
        g["EnemyDataPointer"]["_uid"]["_id"] = s["to"]
    o.save_typetree(t)
    n = _save(env, dst)
    back = [t for _, t in _monos(_load(dst)) if _dungeon(t) and _dungeon(t)["_uid"]["_id"] == want_uid]
    bg = {g["_uid"]["_id"]: g for g in _dungeon(back[0])["_spawnSettings"]["_spawnGroupsEnemy"]}
    for s in swaps:
        if bg[s["spawnGroupId"]]["EnemyDataPointer"]["_uid"]["_id"] != s["to"]:
            raise SystemExit("readback: group %s did not keep the swap" % s["spawnGroupId"])
    print("  dungeonsettings: %s  %d swap(s) read back  (%d bytes)"
          % (t["m_Name"], len(swaps), n))
    return n


def widen_bands(src, dst, definition):
    bands = definition.get("enemyBands", [])
    if not bands:
        return None
    env = _load(src)
    by = {t.get("_uid", {}).get("_id"): (o, t) for o, t in _monos(env)
          if isinstance(t.get("_variants"), list)}
    for b in bands:
        if b["enemyDataId"] not in by:
            raise SystemExit("EnemyData %s not in %s" % (b["enemyDataId"], src))
        o, t = by[b["enemyDataId"]]
        last = t["_variants"][-1]
        if last["_maxLevel"] not in (b["fromMaxLevel"], b["maxLevel"]):
            raise SystemExit("%s top band ends at %d, definition expects %d — refusing"
                             % (t["m_Name"], last["_maxLevel"], b["fromMaxLevel"]))
        last["_maxLevel"] = b["maxLevel"]
        o.save_typetree(t)
    n = _save(env, dst)
    back = {t.get("_uid", {}).get("_id"): t for _, t in _monos(_load(dst))
            if isinstance(t.get("_variants"), list)}
    for b in bands:
        if back[b["enemyDataId"]]["_variants"][-1]["_maxLevel"] != b["maxLevel"]:
            raise SystemExit("readback: %s band not widened" % b["enemyDataId"])
    print("  enemies_sab: %d band(s) widened, read back  (%d bytes)" % (len(bands), n))
    return n


def set_loc(src, dst, definition):
    loc = definition.get("loc", {})
    if not loc:
        return None
    prefix = definition.get("prefix", "Newblades:")
    for key, text in loc.items():
        if not text.startswith(prefix):
            raise SystemExit("loc %s = %r does not carry the %r prefix — new content must"
                             % (key, text, prefix))
    env = _load(src)
    locales = [(o, t) for o, t in _monos(env) if isinstance(t.get("entries"), list)]
    if not locales:
        raise SystemExit("no LocalizationData in %s" % src)
    expect = {}
    for o, t in locales:
        idx = {e["key"]: e for e in t["entries"]}
        for key, text in loc.items():
            stock = idx[key]["entry"] if key in idx else ""
            if stock.startswith(prefix):          # already applied: idempotent
                new = stock
            else:
                new = text.replace("{retail}", stock)
            if key in idx:
                idx[key]["entry"] = new
            else:
                t["entries"].append({"key": key, "entry": new})
            expect[(t["m_Name"], key)] = new
        o.save_typetree(t)
    n = _save(env, dst)
    for _, t in _monos(_load(dst)):
        if not isinstance(t.get("entries"), list):
            continue
        idx = {e["key"]: e["entry"] for e in t["entries"]}
        for key in loc:
            if idx.get(key) != expect[(t["m_Name"], key)]:
                raise SystemExit("readback: %s/%s did not keep its text" % (t["m_Name"], key))
    en = [t for _, t in locales if t["m_Name"] == "en-US"]
    shown = {k: expect[("en-US", k)] for k in loc} if en else {}
    print("  localization: %d key(s) x %d locales read back  en-US %s  (%d bytes)"
          % (len(loc), len(locales), shown, n))
    return n


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("definition")
    ap.add_argument("--dungeonsettings")
    ap.add_argument("--enemies")
    ap.add_argument("--localization")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    d = json.load(open(a.definition))
    if not d.get("id", "").startswith("nb-"):
        raise SystemExit("content id must start with 'nb-' (got %r)" % d.get("id"))
    os.makedirs(a.out, exist_ok=True)
    made = {}
    for key, fn, src, name in (("enemySwaps", swap_enemies, a.dungeonsettings, "dungeonsettings"),
                               ("enemyBands", widen_bands, a.enemies, "enemies_sab"),
                               ("loc", set_loc, a.localization, "localization")):
        if not d.get(key):
            continue
        if not src:
            raise SystemExit("%s needs --%s" % (key, name.split("_")[0]))
        dst = os.path.join(a.out, name)
        fn(src, dst, d)
        made[name] = dst
    print("  build_pack_v2 args: " + " ".join("%s=%s" % kv for kv in made.items()))


if __name__ == "__main__":
    sys.exit(main())
