#!/usr/bin/env python3
"""Switch on the Witches Festival theme Bethesda already shipped, bundle by bundle.

The hand-made pack (`halloween.py`, `night_sky.py`, `night_lights.py`) grades
stock art with values we chose. This one authors nothing: every value it writes
is copied from a night variant that ships in the client and was never selected.

    kits_sab                        TownKit     <- TownKit_Night      lighting + audio
                                    MainMenuKit <- MainMenuKit_Night  lighting
    levels_mainmenu_mainmenu        Night_VFX_&_Lights on, Day_VFX_&_Lights off
    levels_districts_towndistrict_0 TownKitSun rig <- townkitnight.prefab's moon

Why each is shaped the way it is:

* **kits_sab ships in the APK and has never been downloaded**, but only because
  its APK copy hashes the same as the manifest entry. 52 of the APK's 116 built-in
  bundles hash differently and ARE downloaded (audio_sab, skybox_sab, common...),
  so a new hash in a pack manifest is expected to make the client fetch this one
  too. `_ExtraData` holds its record (size 16,596 = the APK copy, byte for byte).
* The kit is patched in place rather than repointing `LevelData._kitList` in
  `gameplaymetadata`: that bundle is 1.1 MB of every level's data, and the one
  device A/B we have (2026-09-21, Pixel) broke character loading when it was
  rebuilt into an APK. The kit object is small and only lighting/audio change.
  Its uid and name are kept, so every lookup still finds "TownKit".
* `TownKit_Night._audioSetting` IS `AudioKitTown_WitchFestivalSetting` — the
  spooky town music comes with the kit, no audio_sab patch needed.
* The menu scene carries `Day_VFX_&_Lights`, `Night_VFX_&_Lights` and
  `Winter_VFX_&_Lights` side by side, Night and Winter inactive. Flipping two
  `m_IsActive` flags is the whole change.
* `townkitnight.prefab` is a container asset of the MENU bundle, with the same
  root transform as the town's `TownKitSun` — a drop-in replacement for it. A
  scene cannot reference another bundle's object without a new external, so the
  moon light's values are copied onto the town's own sun light instead, and the
  mountain fill light (which the night rig does not have) is switched off.

    ./builtin_halloween.py kits  <kits_sab in> <out>
    ./builtin_halloween.py menu  <levels_mainmenu_mainmenu in> <out>
    ./builtin_halloween.py town  <levels_districts_towndistrict_0 in> <menu bundle> <out>

Every subcommand refuses rather than writing when it cannot find exactly what
it expects, and reads its own output back before reporting success.
"""
import copy
import os
import sys

import UnityPy

KIT_PAIRS = {  # patched kit -> the night kit it takes its values from
    "TownKit": "TownKit_Night",
    "MainMenuKit": "MainMenuKit_Night",
}
#: Only these kit fields change. uid, name, room sizes, doors: untouched.
KIT_FIELDS = {"TownKit": ("_lightingSettings", "_audioSetting"),
              "MainMenuKit": ("_lightingSettings", "_audioSetting")}

MENU_ON, MENU_OFF = "Night_VFX_&_Lights", "Day_VFX_&_Lights"

#: The standalone night rig in the menu bundle's container, and the town's sun.
NIGHT_RIG_PATH = "assets/bgs/art/sun/townkitnight.prefab"
LIGHT_FIELDS = ("m_Color", "m_Intensity", "m_Shadows", "m_BounceIntensity")


def _save(env, dst):
    data = env.file.save(packer="original")
    with open(dst, "wb") as fh:
        fh.write(data)
    return len(data)


def _trees(env, type_name):
    out = []
    for o in env.objects:
        if o.type.name != type_name:
            continue
        try:
            out.append((o, o.read_typetree()))
        except Exception:
            continue
    return out


def kits(src, dst):
    env = UnityPy.load(src)
    by = {t.get("m_Name"): (o, t) for o, t in _trees(env, "MonoBehaviour")}
    for target, donor in KIT_PAIRS.items():
        if target not in by or donor not in by:
            raise SystemExit("kits: %s or %s missing — not the kits_sab we measured"
                             % (target, donor))
        o, t = by[target]
        src_kit = by[donor][1]["_kit"]
        for field in KIT_FIELDS[target]:
            t["_kit"][field] = copy.deepcopy(src_kit[field])
        o.save_typetree(t)
    n = _save(env, dst)
    back = {t.get("m_Name"): t for _, t in _trees(UnityPy.load(dst), "MonoBehaviour")}
    for target, donor in KIT_PAIRS.items():
        for field in KIT_FIELDS[target]:
            if back[target]["_kit"][field] != back[donor]["_kit"][field]:
                raise SystemExit("kits: %s.%s did not read back as %s's" % (target, field, donor))
        if back[target]["_kit"]["_uid"] == back[donor]["_kit"]["_uid"]:
            raise SystemExit("kits: %s took the night kit's uid — lookups would break" % target)
    print("  kits_sab: TownKit, MainMenuKit now carry their _Night lighting+audio (%d bytes)" % n)


def _gameobjects(env):
    return {t["m_Name"]: (o, t) for o, t in _trees(env, "GameObject")}


def menu(src, dst):
    env = UnityPy.load(src)
    gos = [(o, t) for o, t in _trees(env, "GameObject") if t["m_Name"] in (MENU_ON, MENU_OFF)]
    names = sorted(t["m_Name"] for _, t in gos)
    if names != sorted([MENU_ON, MENU_OFF]):
        raise SystemExit("menu: expected exactly one %s and one %s, found %s" % (MENU_ON, MENU_OFF, names))
    for o, t in gos:
        t["m_IsActive"] = t["m_Name"] == MENU_ON
        o.save_typetree(t)
    n = _save(env, dst)
    back = _gameobjects(UnityPy.load(dst))
    if not back[MENU_ON][1]["m_IsActive"] or back[MENU_OFF][1]["m_IsActive"]:
        raise SystemExit("menu: the flags did not read back")
    print("  levels_mainmenu_mainmenu: %s on, %s off (%d bytes)" % (MENU_ON, MENU_OFF, n))


def _rig(env, root_pid):
    """{child name: (Transform obj, tree, Light obj|None, tree|None, GameObject obj, tree)}"""
    objs = {o.path_id: o for o in env.objects}
    root = objs[root_pid].read_typetree()
    xf = next(objs[c["component"]["m_PathID"]] for c in root["m_Component"]
              if objs[c["component"]["m_PathID"]].type.name == "Transform")
    out = {}
    for ch in xf.read_typetree()["m_Children"]:
        cxo = objs[ch["m_PathID"]]
        cxt = cxo.read_typetree()
        go = objs[cxt["m_GameObject"]["m_PathID"]]
        got = go.read_typetree()
        light = next((objs[c["component"]["m_PathID"]] for c in got["m_Component"]
                      if objs[c["component"]["m_PathID"]].type.name == "Light"), None)
        out[got["m_Name"]] = (cxo, cxt, light, light.read_typetree() if light else None, go, got)
    return out


def town(src, menu_src, dst):
    menu_env = UnityPy.load(menu_src)
    bundle = next(t for o, t in _trees(menu_env, "AssetBundle"))
    night_pid = next(c[1]["asset"]["m_PathID"] for c in bundle["m_Container"] if c[0] == NIGHT_RIG_PATH)
    night = _rig(menu_env, night_pid)
    if list(night) != ["TownKitMoonLight"]:
        raise SystemExit("town: the night rig is not one moon light: %s" % list(night))
    _, moon_xf, _, moon_light, _, _ = night["TownKitMoonLight"]

    env = UnityPy.load(src)
    suns = [(o, t) for o, t in _trees(env, "GameObject") if t["m_Name"] == "TownKitSun"]
    if len(suns) != 1:
        raise SystemExit("town: expected one TownKitSun, found %d" % len(suns))
    rig = _rig(env, suns[0][0].path_id)
    if set(rig) != {"TownKitSunLight", "TownMountainSunLight"}:
        raise SystemExit("town: TownKitSun children changed: %s" % sorted(rig))

    sxo, sxt, slo, slt, _, _ = rig["TownKitSunLight"]
    for f in LIGHT_FIELDS:
        slt[f] = copy.deepcopy(moon_light[f])
    slo.save_typetree(slt)
    sxt["m_LocalRotation"] = copy.deepcopy(moon_xf["m_LocalRotation"])
    sxo.save_typetree(sxt)
    # The night rig has no mountain fill light; switch it off rather than delete it.
    _, _, _, _, mgo, mgt = rig["TownMountainSunLight"]
    mgt["m_IsActive"] = False
    mgo.save_typetree(mgt)

    n = _save(env, dst)
    env2 = UnityPy.load(dst)
    sun2 = next(o for o, t in _trees(env2, "GameObject") if t["m_Name"] == "TownKitSun")
    back = _rig(env2, sun2.path_id)
    got = back["TownKitSunLight"][3]
    if any(got[f] != moon_light[f] for f in LIGHT_FIELDS) or back["TownMountainSunLight"][5]["m_IsActive"]:
        raise SystemExit("town: the moon light did not read back")
    print("  levels_districts_towndistrict_0: TownKitSunLight = moon (%.2f, %.2f, %.2f) x%.2f; "
          "mountain light off (%d bytes)" % (got["m_Color"]["r"], got["m_Color"]["g"],
                                             got["m_Color"]["b"], got["m_Intensity"], n))


if __name__ == "__main__":
    cmd, args = sys.argv[1], sys.argv[2:]
    {"kits": kits, "menu": menu, "town": town}[cmd](*args)
