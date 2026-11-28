#!/usr/bin/env python3
"""Turn a scene's directional lights to moonlight.

The palette grade recoloured surfaces and left the town in bright daylight,
which reads as a rendering bug rather than a season -- purple roofs under a blue
summer sky. What actually makes retail's Halloween look like Halloween is that
the sun is off.

The skybox itself ships inside the APK and cannot be replaced over the air, but
the scene's lights do not: `levels_districts_towndistrict_0` is remote and holds
two directional lights (`m_Type` 1). Cooling them to moonlight and dropping the
intensity darkens everything the sun touches, which is the whole town.

    ./night_lights.py <src bundle> <dst bundle> [intensity_scale]

Only `m_Color` and `m_Intensity` are touched. Shadow settings, range and
everything else stay exactly as authored -- changing shadow bias on a scene this
size is how you get acne on every wall.
"""
import os
import sys

import UnityPy

# A cold, slightly blue moon. Not black: at zero the town is unreadable and
# players cannot find their own buildings.
MOON = {"r": 0.34, "g": 0.42, "b": 0.72, "a": 1.0}


def main():
    src, dst = sys.argv[1], sys.argv[2]
    scale = float(sys.argv[3]) if len(sys.argv) > 3 else 0.28

    env = UnityPy.load(src)
    touched = 0
    for o in env.objects:
        if o.type.name != "Light":
            continue
        tree = o.read_typetree()
        if tree.get("m_Type") != 1:          # directional only; leave lamps alone
            continue
        before = tree.get("m_Intensity")
        tree["m_Color"] = dict(MOON)
        tree["m_Intensity"] = round(before * scale, 4)
        o.save_typetree(tree)
        touched += 1
        print("  light %s: intensity %.3f -> %.3f, colour -> moonlight"
              % (o.path_id, before, tree["m_Intensity"]))

    if not touched:
        raise SystemExit("no directional lights found — refusing to write")

    data = env.file.save(packer="original")
    with open(dst, "wb") as fh:
        fh.write(data)
    print("  wrote %s (%d lights, %d bytes, was %d)"
          % (dst, touched, len(data), os.path.getsize(src)))


if __name__ == "__main__":
    main()
