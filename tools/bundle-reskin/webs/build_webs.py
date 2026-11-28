#!/usr/bin/env python3
"""Author cobwebs for every town building and write the patched bundles.

    ./build_webs.py <stock town dir> <vfx_sab> <animatedobjects_breakables_sab> <out dir> [name filter ...]

<stock town dir> holds the stock building bundles (town_house_*, town_forge_*, ...)
and `town_houses_sab` & co., whose meshes the prefabs reference. Every bundle the
filter admits gets one `Halloween_Webs` child (see webmesh.py for the placements,
graft.py for how it is inserted). Deterministic: the same inputs give the same
bytes. `preview.py` renders the placements offline before a device round.

The arches have no lit renderer to clone and are skipped (reported, not fatal).
"""
import os
import re
import sys
from multiprocessing import Pool

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import graft  # noqa: E402
import housegeo  # noqa: E402
import webmesh  # noqa: E402

FAMILIES = re.compile(r'^town_(alchemistshop|enchanter|forge|house_[abc]|townhall_townhall|workshop|'
                      r'maingate_maingate|arches|alcove).*_0[0-2]$')

#: BGS/Particles/AlphaBlend in shaders_sab: unlit, so the webbing reads pale in a
#: night town. Bethesda's own lit see-through webbing was measured on the
#: emulator (rounds 1-2) and is near-invisible under the moonlight rig at any _Color.
ALPHA_BLEND = 5270646105750517290
MATERIAL = dict(color=(0.8, 0.85, 1.0, 1.0), tint=(0.40, 0.43, 0.52, 0.5), shader_pid=ALPHA_BLEND,
                lightmap=False)
#: Vertex colour is the lever that tints the particle shader: moonlit blue-white.
VERTEX_COLOR = (175, 192, 235, 235)


def kind_of(name):
    return 'townhall' if 'townhall' in name else 'gate' if 'maingate' in name else 'house'


def one(job):
    name, town, vfx, brk, out = job
    try:
        parts = housegeo.load(os.path.join(town, name), town)
        sheet, placed, _, _ = webmesh.dress(parts, kind_of(name), seed=name)
        V, N, UV, C, I = sheet.arrays(color=VERTEX_COLOR)
        size, tmpl = graft.graft(os.path.join(town, name), os.path.join(out, name), vfx, brk,
                                 V, N, UV, C, I, seed=name, **MATERIAL)
        return name, [p[0] for p in placed], len(V), size, None
    except BaseException as e:  # noqa: BLE001 - report every building, keep going
        return name, [], 0, 0, '%s: %s' % (type(e).__name__, e)


def main(argv):
    town, vfx, brk, out = argv[:4]
    filters = argv[4:]
    os.makedirs(out, exist_ok=True)
    names = sorted(n for n in os.listdir(town) if FAMILIES.match(n))
    if filters:
        names = [n for n in names if any(f in n for f in filters)]
    failed = []
    with Pool(min(6, os.cpu_count() or 1)) as pool:
        for name, placed, nv, size, err in pool.imap(one, [(n, town, vfx, brk, out) for n in names]):
            if err:
                failed.append(name)
                print('  %-46s SKIPPED  %s' % (name, err))
            else:
                print('  %-46s %-42s %5d verts %8d B' % (name, ','.join(placed), nv, size))
    print('%d dressed, %d skipped' % (len(names) - len(failed), len(failed)))


if __name__ == '__main__':
    main(sys.argv[1:])
