#!/usr/bin/env python3
"""Darken the distant backdrop the town is painted against.

`night_sky.py` takes the sky itself to night and `night_lights.py` turns the
sun to moonlight, and between them the town still showed a bright blue-green
mountain range above the horizon. That range is not sky and not lit by the
sun: it is real geometry with its own material, so neither of the other two
scripts could ever have reached it.

Which material was measured, not guessed. Resolving every m_Materials PPtr in
`levels_districts_towndistrict_0` that lands in `landscape_sab` gives 18
distinct materials, and exactly one of them is a background card:
`Tree_BackGround_Snow`, 37 references. The rest are grass, flowers and trees
the player stands next to — dimming those is the lights' job, not this one's.

Its `_ColorTint` is already the cool distant blue the artists authored, which
is why the range reads as haze rather than foliage. What was left at full
strength is `_Color` (1, 1, 1) and `_AmbientTweak`, and those are what keep it
at daylight brightness after everything else has gone dark.

    ./night_landscape.py <src bundle> <dst bundle> [scale]
"""
import os
import sys

import UnityPy

#: The distant-backdrop materials. `Tree_BackGround_Snow` is the town
#: district's (37 refs); `Tree_BackGround` is the one the main menu's forest
#: LOD uses (137 of its 142 renderers), which is the range visible behind the
#: castle on the hub.
BACKDROP = ("Tree_BackGround_Snow", "Tree_BackGround")

#: Multiplicative, so the authored cool tint keeps its hue and only loses
#: brightness. 0.25 lands the range near the darker roofs instead of above the
#: sky, which is the whole complaint.
DEFAULT_SCALE = 0.25

#: Both terms matter: _Color is the albedo multiplier and _AmbientTweak is how
#: much ambient the card takes. Scaling only one leaves the other at full.
SCALED = ("_Color", "_AmbientTweak", "_ColorTint")


def darken(src, dst, scale=DEFAULT_SCALE):
    env = UnityPy.load(src)
    touched = []
    for obj in env.objects:
        if obj.type.name != "Material":
            continue
        tree = obj.read_typetree()
        name = tree.get("m_Name")
        if name not in BACKDROP:
            continue
        colors = tree["m_SavedProperties"].get("m_Colors") or []
        for i, (key, value) in enumerate(colors):
            if key in SCALED:
                colors[i] = (key, dict(
                    value, **{ch: round(value[ch] * scale, 6) for ch in "rgb"}))
        obj.save_typetree(tree)
        touched.append(name)

    missing = [n for n in BACKDROP if n not in touched]
    if missing:
        raise SystemExit(
            "night_landscape: expected materials not found: %s — the bundle "
            "layout changed and this script would silently do nothing"
            % ", ".join(missing))

    data = env.file.save(packer="original")
    with open(dst, "wb") as fh:
        fh.write(data)
    print("  %s: %d material(s) at %.2f, %d bytes (was %d)"
          % (os.path.basename(dst), len(touched), scale, len(data),
             os.path.getsize(src)))


if __name__ == "__main__":
    darken(sys.argv[1], sys.argv[2],
           float(sys.argv[3]) if len(sys.argv) > 3 else DEFAULT_SCALE)
