#!/usr/bin/env python3
"""Render a building with its authored webs, four views, before any device round.

    ./preview.py <stock town dir> <out.png> <building bundle name> [...]
"""
import math
import os
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import housegeo  # noqa: E402
import webmesh  # noqa: E402
import zrender  # noqa: E402
from build_webs import kind_of  # noqa: E402


def main(town, out, names):
    from PIL import Image
    rows = []
    for name in names:
        parts = housegeo.load(os.path.join(town, name), town)
        sheet, placed, _, _ = webmesh.dress(parts, kind_of(name), seed=name)
        allv = np.concatenate([q[2] for q in parts])
        lo, hi = allv.min(0), allv.max(0)
        c, R = (lo + hi) / 2, np.linalg.norm(hi - lo)
        views = [((c[0] + R * math.cos(math.radians(a)), lo[1] + 1.7, c[2] + R * math.sin(math.radians(a))),
                  (c[0], c[1] * 0.8, c[2])) for a in (35, 125, 215, 305)]
        tmp = out + '.%s.png' % name
        zrender.render(parts, [(np.array(sheet.V), np.array(sheet.I))], tmp, views, W=360, H=300)
        rows.append(Image.open(tmp))
        os.remove(tmp)
        print(name, [p[0] for p in placed])
    img = Image.new('RGB', (rows[0].width, sum(r.height for r in rows)))
    y = 0
    for r in rows:
        img.paste(r, (0, y))
        y += r.height
    img.save(out)


if __name__ == '__main__':
    main(sys.argv[1], sys.argv[2], sys.argv[3:])
