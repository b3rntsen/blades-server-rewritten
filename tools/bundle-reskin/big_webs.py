#!/usr/bin/env python3
"""Enlarge a few ivy placements so the webbing has big draped sheets too.

Retail's town has webs at two scales: small ones clinging to walls, and a few
large sheets hanging from eaves down to the ground. Re-texturing the ivy gives
the small ones for free, because ivy is already scattered over the walls -- but
every patch is ivy-sized, so the big ones are missing.

They can be had without authoring anything: scale up a handful of the existing
placements. Scaling the placement scales its whole LOD group with it, so the
mesh stays correct at every distance.

Only `Ivy_0N` placements are touched, never the `L2_Ivy` LOD children inside
them -- scaling both would compound and produce one absurd sheet. The chosen few
are spread apart by world position rather than taken in file order, so the big
webs land on different buildings instead of stacking on one wall.

    ./big_webs.py <src> <dst> [count] [factor]
"""
import math
import sys

import UnityPy


def main():
    src, dst = sys.argv[1], sys.argv[2]
    count = int(sys.argv[3]) if len(sys.argv) > 3 else 6
    factor = float(sys.argv[4]) if len(sys.argv) > 4 else 3.2

    env = UnityPy.load(src)
    gname = {}
    for o in env.objects:
        if o.type.name == "GameObject":
            try:
                gname[o.path_id] = getattr(o.read(), "m_Name", "")
            except Exception:
                pass

    cands = []
    for o in env.objects:
        if o.type.name != "Transform":
            continue
        t = o.read_typetree()
        nm = gname.get((t.get("m_GameObject") or {}).get("m_PathID"), "")
        # the placements, not the LOD children they contain
        if not nm.lower().startswith("ivy_"):
            continue
        p = t.get("m_LocalPosition") or {}
        cands.append((o, t, nm, (p.get("x", 0.0), p.get("y", 0.0), p.get("z", 0.0))))

    if not cands:
        raise SystemExit("no ivy placements found — refusing to write")

    # Farthest-point pick: start from the first, then repeatedly take whichever
    # candidate is furthest from everything already chosen.
    chosen = [cands[0]]
    while len(chosen) < min(count, len(cands)):
        best, bestd = None, -1.0
        for c in cands:
            if c in chosen:
                continue
            d = min(math.dist(c[3], k[3]) for k in chosen)
            if d > bestd:
                best, bestd = c, d
        chosen.append(best)

    for o, t, nm, pos in chosen:
        s = t.get("m_LocalScale") or {}
        for axis in ("x", "y", "z"):
            s[axis] = round(s.get(axis, 1.0) * factor, 4)
        t["m_LocalScale"] = s
        o.save_typetree(t)
        print("  %-22s at (%.0f, %.0f, %.0f) -> scale %.2f"
              % (nm, pos[0], pos[1], pos[2], s["x"]))

    data = env.file.save(packer="original")
    open(dst, "wb").write(data)
    print("  wrote %s (%d enlarged, %d bytes)" % (dst, len(chosen), len(data)))


if __name__ == "__main__":
    main()
