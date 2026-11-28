"""Author cobweb sheets fitted to a building's real geometry.

Retail (2022 Witches Festival capture, HKHvv26lTOo) dresses every building with
three forms of one translucent sheet web:
  1. corner tents  - from high on a wall corner down to the ground,
  2. drapes        - from a chimney / tower top down over the roof and past the eave,
  3. eave curtains - sagging under a roof edge.
Everything here works in prefab-root space (metres, Y up) against a top-down
heightfield of the building's own triangles, so sheets rest ON roofs and stay
OUTSIDE walls instead of floating or clipping.
"""
import math
import random

import numpy as np

CELL = 0.15


class Field:
    def __init__(self, parts):
        tris = np.concatenate([v[f] for _, _, v, f in parts])
        lo = tris.reshape(-1, 3).min(0) - 1.0
        hi = tris.reshape(-1, 3).max(0) + 1.0
        self.x0, self.z0 = lo[0], lo[2]
        self.ground = float(np.percentile(tris[:, :, 1], 1))
        nx = int((hi[0] - lo[0]) / CELL) + 1
        nz = int((hi[2] - lo[2]) / CELL) + 1
        H = np.full((nx, nz), -1e9)
        for t in tris:
            xs, zs = t[:, 0], t[:, 2]
            i0, i1 = int((xs.min() - self.x0) / CELL), int((xs.max() - self.x0) / CELL) + 1
            j0, j1 = int((zs.min() - self.z0) / CELL), int((zs.max() - self.z0) / CELL) + 1
            if i1 - i0 > 400 or j1 - j0 > 400:
                continue
            gi, gj = np.meshgrid(np.arange(i0, i1 + 1), np.arange(j0, j1 + 1), indexing="ij")
            px = self.x0 + gi * CELL
            pz = self.z0 + gj * CELL
            a, b, c = t
            d = (b[2] - c[2]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[2] - c[2])
            if abs(d) < 1e-9:
                continue
            l1 = ((b[2] - c[2]) * (px - c[0]) + (c[0] - b[0]) * (pz - c[2])) / d
            l2 = ((c[2] - a[2]) * (px - c[0]) + (a[0] - c[0]) * (pz - c[2])) / d
            l3 = 1 - l1 - l2
            inside = (l1 >= -1e-3) & (l2 >= -1e-3) & (l3 >= -1e-3)
            y = l1 * a[1] + l2 * b[1] + l3 * c[1]
            gi, gj, y = gi[inside], gj[inside], y[inside]
            ok = (gi >= 0) & (gi < nx) & (gj >= 0) & (gj < nz)
            np.maximum.at(H, (gi[ok], gj[ok]), y[ok])
        H[H < self.ground] = self.ground
        self.H = H
        self.nx, self.nz = nx, nz

    def h(self, x, z):
        i = np.clip(((np.asarray(x) - self.x0) / CELL).round().astype(int), 0, self.nx - 1)
        j = np.clip(((np.asarray(z) - self.z0) / CELL).round().astype(int), 0, self.nz - 1)
        return self.H[i, j]

    def xz(self, i, j):
        return self.x0 + i * CELL, self.z0 + j * CELL


def hull(pts):
    pts = sorted(set(map(tuple, pts)))
    if len(pts) < 3:
        return pts

    def cross(o, a, b):
        return (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    lo, up = [], []
    for p in pts:
        while len(lo) >= 2 and cross(lo[-2], lo[-1], p) <= 0:
            lo.pop()
        lo.append(p)
    for p in reversed(pts):
        while len(up) >= 2 and cross(up[-2], up[-1], p) <= 0:
            up.pop()
        up.append(p)
    return lo[:-1] + up[:-1]


def simplify(poly, tol=0.35):
    """Drop hull vertices that turn less than ~20 degrees or sit closer than tol."""
    out = list(poly)
    changed = True
    while changed and len(out) > 4:
        changed = False
        for k in range(len(out)):
            a, b, c = np.array(out[k - 1]), np.array(out[k]), np.array(out[(k + 1) % len(out)])
            u, v = b - a, c - b
            ang = math.degrees(math.acos(np.clip(u @ v / (np.linalg.norm(u) * np.linalg.norm(v) + 1e-9), -1, 1)))
            if ang < 20 or np.linalg.norm(u) < tol:
                out.pop(k)
                changed = True
                break
    return out


class Sheet:
    """A grid of vertices -> double-sided triangles with UVs in metres/tile."""

    def __init__(self):
        self.V, self.UV, self.I = [], [], []

    def add_grid(self, P, tile=2.2, keep=None):
        rows, cols = P.shape[:2]
        base = len(self.V)
        # UV by accumulated distance so the texture never stretches
        du = np.zeros((rows, cols))
        dv = np.zeros((rows, cols))
        du[:, 1:] = np.cumsum(np.linalg.norm(np.diff(P, axis=1), axis=2), axis=1)
        dv[1:, :] = np.cumsum(np.linalg.norm(np.diff(P, axis=0), axis=2), axis=0)
        for r in range(rows):
            for c in range(cols):
                self.V.append(P[r, c])
                self.UV.append((du[r, c] / tile, dv[r, c] / tile))
        for r in range(rows - 1):
            for c in range(cols - 1):
                if keep is not None and not keep(r, c):
                    continue
                a = base + r * cols + c
                b, d, e = a + 1, a + cols, a + cols + 1
                self.I.append((a, d, b))
                self.I.append((b, d, e))

    def arrays(self, color=(138, 138, 138, 255)):
        V = np.array(self.V, np.float32)
        UV = np.array(self.UV, np.float32)
        I = np.array(self.I, np.int64)
        tri = V[I]
        fn = np.cross(tri[:, 1] - tri[:, 0], tri[:, 2] - tri[:, 0])
        # a tent's apex row is one point: its triangles have no area, draw nothing
        keep = np.linalg.norm(fn, axis=1) > 1e-8
        I, fn = I[keep], fn[keep]
        # double-sided: duplicate vertices with flipped normals
        n = np.zeros_like(V)
        for k in range(3):
            np.add.at(n, I[:, k], fn)
        ln = np.linalg.norm(n, axis=1)
        n[ln < 1e-8] = (0.0, 1.0, 0.0)     # a vertex only degenerate triangles touched
        n /= np.linalg.norm(n, axis=1)[:, None]
        V2 = np.concatenate([V, V])
        N2 = np.concatenate([n, -n])
        UV2 = np.concatenate([UV, UV])
        I2 = np.concatenate([I, I[:, ::-1] + len(V)]).reshape(-1)
        C = np.tile(np.array(color, np.uint8), (len(V2), 1))
        assert len(V2) < 65535
        return V2, N2.astype(np.float32), UV2, C, I2


def _noise(rng, n, amp):
    k = np.arange(n)
    ph = [rng.uniform(0, 6.28) for _ in range(3)]
    return amp * (0.5 * np.sin(k * 0.9 + ph[0]) + 0.3 * np.sin(k * 2.3 + ph[1]) + 0.2 * np.sin(k * 5.1 + ph[2]))


def tent(sheet, F, corner, out_dir, wall_a, wall_b, apex_h, rng, rows=10, cols=16, poly=None, solid=None):
    """Corner tent: apex high on the corner edge, base an arc on the ground wrapping it."""
    cx, cz = corner
    g = F.ground
    R = float(np.clip(0.6 * (apex_h - g), 1.4, 3.0))
    A = np.array([cx + out_dir[0] * 0.12, apex_h, cz + out_dir[1] * 0.12])
    # base arc from along wall_a (pointing back along the wall) around to wall_b
    a0 = math.atan2(wall_a[1], wall_a[0])
    a1 = math.atan2(wall_b[1], wall_b[0])
    # sweep through the outward direction
    ao = math.atan2(out_dir[1], out_dir[0])

    def norm(a):
        while a < ao - math.pi:
            a += 2 * math.pi
        while a > ao + math.pi:
            a -= 2 * math.pi
        return a
    a0, a1 = norm(a0), norm(a1)
    if a0 > a1:
        a0, a1 = a1, a0
    # pull the ends off the walls a little so the sheet meets the wall, not the wall plane
    ang = np.linspace(a0 + 0.03, a1 - 0.03, cols)
    rad = R * (1 + _noise(rng, cols, 0.18))
    # the ends run back along the walls only a short way: a cone hugging the corner
    taper = np.interp(np.arange(cols), [0, 3, cols - 4, cols - 1], [0.6, 0.85, 0.85, 0.6])
    rad = rad * taper / 0.85
    P = np.zeros((rows, cols, 3))
    for c in range(cols):
        B = np.array([cx + math.cos(ang[c]) * rad[c], g + 0.02, cz + math.sin(ang[c]) * rad[c]])
        for r in range(rows):
            t = r / (rows - 1)
            p = A + (B - A) * t
            # gravity sag toward the corner axis
            sag = 0.10 * np.linalg.norm(B - A) * math.sin(math.pi * t)
            hx, hz = cx - p[0], cz - p[2]
            hn = math.hypot(hx, hz) + 1e-9
            p[0] += sag * hx / hn * 0.6
            p[2] += sag * hz / hn * 0.6
            p[1] -= sag * 0.5
            p[1] = max(p[1], g + 0.02)
            P[r, c] = p
    _push_out(F, P, (cx, cz), poly, solid)
    sheet.add_grid(P)


def inside_poly(poly, x, z):
    """Convex polygon (CCW hull) containment."""
    n = len(poly)
    sgn = 0
    for k in range(n):
        a, b = poly[k], poly[(k + 1) % n]
        c = (b[0] - a[0]) * (z - a[1]) - (b[1] - a[1]) * (x - a[0])
        if c < 0:
            return False
    return True


def _push_out(F, P, center, poly=None, solid=None, margin=0.08):
    """Move vertices that ended up inside the walls outward, away from `center`."""
    if solid is not None:
        def inside(x, z):
            i = int(round((x - F.x0) / CELL)); j = int(round((z - F.z0) / CELL))
            return 0 <= i < F.nx and 0 <= j < F.nz and solid[i, j]
    elif poly is not None:
        inside = lambda x, z: inside_poly(poly, x, z)  # noqa: E731
    else:
        return
    for idx in np.ndindex(P.shape[:2]):
        p = P[idx]
        for _ in range(60):
            if not inside(p[0], p[2]):
                break
            d = np.array([p[0] - center[0], p[2] - center[1]])
            n = np.linalg.norm(d)
            d = d / n if n > 1e-6 else np.array([1.0, 0.0])
            p[0] += d[0] * 0.05
            p[2] += d[1] * 0.05


def drape(sheet, F, top, bottom_mid, width_top, width_bot, rng, lip=1.6, rows=18, cols=12):
    """Sheet hanging from `top` (chimney/tower rim) down over the roof, past the eave."""
    top = np.asarray(top, float)
    bm = np.asarray(bottom_mid, float)
    d = bm[[0, 2]] - top[[0, 2]]
    L = np.linalg.norm(d)
    d /= L + 1e-9
    side = np.array([-d[1], d[0]])
    # extend beyond the eave: the lip hangs straight down outside the roof edge
    P = np.zeros((rows, cols, 3))
    wob = _noise(rng, cols, 0.25)
    for c in range(cols):
        s = c / (cols - 1) - 0.5
        for r in range(rows):
            t = r / (rows - 1)
            reach = t * (L + 0.35)            # slightly past the eave edge
            w = width_top + (width_bot - width_top) * min(1.0, t * 1.15)
            x = top[0] + d[0] * reach + side[0] * s * w
            z = top[2] + d[1] * reach + side[1] * s * w
            line_y = top[1] + (bm[1] - top[1]) * t
            surf = F.h(x, z)
            y = max(line_y - 0.6 * math.sin(math.pi * min(t, 1.0)), surf + 0.07)
            if surf <= F.ground + 0.3:            # past the edge: hang
                edge_y = P[r - 1, c, 1] if r else line_y
                y = edge_y - (lip * (1 + wob[c])) / (rows * 0.25)
                y = max(y, F.ground + 0.3)
            P[r, c] = (x, y, z)
    sheet.add_grid(P)


def curtain(sheet, F, p0, p1, drop, out, rng, rows=7, cols=14):
    """Eave curtain: top edge along the roof edge, bottom sagging in a catenary."""
    p0, p1 = np.asarray(p0, float), np.asarray(p1, float)
    wob = _noise(rng, cols, 0.2)
    P = np.zeros((rows, cols, 3))
    for c in range(cols):
        s = c / (cols - 1)
        top = p0 + (p1 - p0) * s
        dd = drop * (math.sin(math.pi * s) ** 0.8) * (1 + wob[c]) + 0.15
        for r in range(rows):
            t = r / (rows - 1)
            belly = 0.18 * math.sin(math.pi * t) * math.sin(math.pi * s)
            P[r, c] = (top[0] + out[0] * (0.06 + belly), top[1] - dd * t, top[2] + out[1] * (0.06 + belly))
    sheet.add_grid(P)


def footprint(F, lo=1.2):
    m = F.H > F.ground + lo
    ii, jj = np.nonzero(m)
    return m, [F.xz(i, j) for i, j in zip(ii, jj)]


def _local_median(H, r):
    pad = np.pad(H, r, mode="edge")
    from numpy.lib.stride_tricks import sliding_window_view
    w = sliding_window_view(pad, (2 * r + 1, 2 * r + 1))
    return np.median(w.reshape(H.shape[0], H.shape[1], -1), axis=2)


def chimneys(F):
    """Narrow columns standing clear of the roof around them."""
    m, _ = footprint(F)
    med = _local_median(F.H, 7)
    bump = (F.H - med > 0.7) & m
    seen = np.zeros_like(bump)
    out = []
    for i, j in zip(*np.nonzero(bump)):
        if seen[i, j]:
            continue
        stack, comp = [(i, j)], []
        seen[i, j] = True
        while stack:
            a, b = stack.pop()
            comp.append((a, b))
            for da, db in ((1, 0), (-1, 0), (0, 1), (0, -1)):
                x, y = a + da, b + db
                if 0 <= x < F.nx and 0 <= y < F.nz and bump[x, y] and not seen[x, y]:
                    seen[x, y] = True
                    stack.append((x, y))
        area = len(comp) * CELL * CELL
        if 0.1 < area < 4.0:
            k = max(comp, key=lambda q: F.H[q])
            x, z = F.xz(*k)
            out.append((x, F.H[k], z, area))
    out.sort(key=lambda c: -c[1])
    return out


def eave_toward(F, x, z, direction, start_y):
    """Walk from (x,z) along direction until the heightfield drops to the ground."""
    px, pz = x, z
    last = (px, start_y, pz)
    for _ in range(200):
        px += direction[0] * CELL
        pz += direction[1] * CELL
        h = F.h(px, pz)
        if h <= F.ground + 0.5:
            return np.array(last)
        last = (px, h, pz)
    return np.array(last)


def wall_points(parts, F, lo=0.7, hi=2.4, step=0.12):
    """Points sampled on every triangle inside a band above the ground: the walls."""
    out = []
    for _, _, v, f in parts:
        tri = v[f]
        ymin, ymax = tri[:, :, 1].min(1), tri[:, :, 1].max(1)
        keep = (ymax > F.ground + lo) & (ymin < F.ground + hi)
        for t in tri[keep]:
            a, b, c = t
            area = 0.5 * np.linalg.norm(np.cross(b - a, c - a))
            n = int(min(400, max(3, area / (step * step))))
            r1 = np.sqrt(np.random.RandomState(len(out)).rand(n))
            r2 = np.random.RandomState(len(out) + 1).rand(n)
            p = (1 - r1)[:, None] * a + (r1 * (1 - r2))[:, None] * b + (r1 * r2)[:, None] * c
            p = p[(p[:, 1] > F.ground + lo) & (p[:, 1] < F.ground + hi)]
            out.append(p[:, [0, 2]])
    return np.concatenate(out) if out else np.zeros((0, 2))


def _support(P, a, b, near=0.35):
    """Fraction of 0.25 m steps along a->b that have wall points within `near`."""
    L = np.linalg.norm(b - a)
    n = max(2, int(L / 0.25))
    hits = 0
    for t in np.linspace(0, 1, n):
        q = a + (b - a) * t
        if (np.sum((P - q) ** 2, axis=1) < near * near).any():
            hits += 1
    return hits / n


def solid_mask(F, WP):
    """Wall cells, closed, with everything the outside cannot reach filled in."""
    W = np.zeros((F.nx, F.nz), bool)
    ij = ((WP - [F.x0, F.z0]) / CELL).round().astype(int)
    ok = (ij[:, 0] >= 0) & (ij[:, 0] < F.nx) & (ij[:, 1] >= 0) & (ij[:, 1] < F.nz)
    W[ij[ok, 0], ij[ok, 1]] = True
    W = _dilate(W, 1)
    ext = np.zeros_like(W)
    stack = [(0, 0)]
    ext[0, 0] = True
    while stack:
        a, b = stack.pop()
        for da, db in ((1, 0), (-1, 0), (0, 1), (0, -1)):
            x, y = a + da, b + db
            if 0 <= x < F.nx and 0 <= y < F.nz and not ext[x, y] and not W[x, y]:
                ext[x, y] = True
                stack.append((x, y))
    return ~ext


def _dilate(M, r):
    out = M.copy()
    for da in range(-r, r + 1):
        for db in range(-r, r + 1):
            out |= np.roll(np.roll(M, da, 0), db, 1)
    return out


def _box(M, r):
    """Sum of M over a (2r+1)^2 window, zero-padded."""
    P = np.pad(M.astype(np.int32), r)
    c = P.cumsum(0).cumsum(1)
    c = np.pad(c, ((1, 0), (1, 0)))
    n = 2 * r + 1
    return c[n:, n:] - c[:-n, n:] - c[n:, :-n] + c[:-n, :-n]


def convex_corners(F, S, spacing=2.5):
    """Cells on the solid's edge whose neighbourhood is ~3/4 outside: convex corners."""
    r = 4
    ext_frac = _box(~S, r) / float((2 * r + 1) ** 2)
    big = _box(S, 8)
    edge = S & (_box(~S, 1) > 0)
    cand = edge & (ext_frac > 0.6) & (big > 50)
    pts = []
    for i, j in zip(*np.nonzero(cand)):
        pts.append((ext_frac[i, j], i, j))
    pts.sort(reverse=True)
    out = []
    for f, i, j in pts:
        x, z = F.xz(i, j)
        if any(math.hypot(x - q[0], z - q[1]) < spacing for q in out):
            continue
        # outward = mean direction to outside cells nearby
        ii, jj = np.nonzero(~S[max(0, i - r):i + r + 1, max(0, j - r):j + r + 1])
        d = np.array([ii.mean() + max(0, i - r) - i, jj.mean() + max(0, j - r) - j])
        if np.linalg.norm(d) < 1e-6:
            continue
        d /= np.linalg.norm(d)
        out.append((x, z, d, f))
    return out


def towers(F, S, prominence=3.0):
    """Tall tops standing well above the roof around them."""
    med = _local_median(F.H, 20)
    cand = (F.H - med > prominence) & S
    out = []
    order = np.argsort(-F.H[cand])
    ii, jj = np.nonzero(cand)
    for k in order:
        i, j = ii[k], jj[k]
        x, z = F.xz(i, j)
        if any(math.hypot(x - q[0], z - q[2]) < 5.0 for q in out):
            continue
        out.append((x, F.H[i, j], z, med[i, j]))
    return out


def dress(parts, kind="house", seed=0):
    """Pick placements for one building and return the merged web sheet."""
    rng = random.Random(seed)
    F = Field(parts)
    sheet = Sheet()
    m, pts = footprint(F)
    WP = wall_points(parts, F)
    WP = np.unique((WP / CELL).round().astype(int), axis=0) * CELL
    S = solid_mask(F, WP)
    si, sj = np.nonzero(S)
    spts = [F.xz(i, j) for i, j in zip(si[::7], sj[::7])]
    poly = simplify(hull(spts), tol=0.5)
    cen = np.array([np.mean([p[0] for p in spts]), np.mean([p[1] for p in spts])])
    placed = []

    n_tents = {"townhall": 6, "gate": 4}.get(kind, 3)
    cap = {"townhall": 7.5, "gate": 5.5}.get(kind, 5.2)
    for x, z, out, f in convex_corners(F, S):
        if len([p for p in placed if p[0] == "tent"]) >= n_tents:
            break
        b = np.array([x, z])
        if any(np.linalg.norm(b - p[1]) < 3.0 for p in placed):
            continue
        ring = [float(F.h(x + math.cos(t) * 0.5, z + math.sin(t) * 0.5)) for t in np.linspace(0, 6.28, 12)]
        ring = [h for h in ring if h > F.ground + 2.0]
        if not ring:
            continue
        hc = min(ring)
        c, s_ = math.cos(math.radians(135)), math.sin(math.radians(135))
        wa = np.array([-(out[0] * c - out[1] * s_), -(out[0] * s_ + out[1] * c)])
        wb = np.array([-(out[0] * c + out[1] * s_), -(-out[0] * s_ + out[1] * c)])
        apex = F.ground + min(hc - F.ground - 0.25, cap * rng.uniform(0.6, 0.85))
        tent(sheet, F, b, out, wa, wb, apex, rng, solid=S)
        placed.append(("tent", b))

    # towers: long drapes from high on the tower across the lower roof beside it
    if kind in ("townhall", "gate", "house"):
        for (x, y, z, base) in towers(F, S)[: {"townhall": 3, "gate": 2}.get(kind, 1)]:
            best = None
            for a in np.linspace(0, 2 * math.pi, 24, endpoint=False):
                d = (math.cos(a), math.sin(a))
                run, px, pz = 0.0, x, z
                # leave the tower first
                for _ in range(80):
                    px += d[0] * CELL; pz += d[1] * CELL
                    if F.h(px, pz) < y - 2.5:
                        break
                sx, sz = px, pz
                e = eave_toward(F, sx, sz, d, float(F.h(sx, sz)))
                run = math.hypot(e[0] - sx, e[2] - sz)
                if F.h(sx, sz) > F.ground + 2.5 and (best is None or run > best[0]):
                    best = (run, e, (sx, sz))
            if best is None or best[0] < 2.0:
                continue
            run, e, (sx, sz) = best
            top_y = base + (y - base) * 0.62
            d = np.array([e[0] - x, e[2] - z]); d /= np.linalg.norm(d)
            top = (sx - d[0] * 0.6, top_y, sz - d[1] * 0.6)
            drape(sheet, F, top, e, 1.2, float(np.clip(0.5 * run, 2.5, 4.5)), rng, lip=rng.uniform(1.5, 2.8))
            placed.append(("tower", np.array([e[0], e[2]])))

    # drapes: from chimneys (or the highest point) down to the eave
    tops = [(c[0], c[1], c[2]) for c in chimneys(F)][: (2 if kind == "townhall" else 1)]
    if not tops:
        i, j = np.unravel_index(np.argmax(F.H), F.H.shape)
        x, z = F.xz(i, j)
        tops = [(x, F.H[i, j], z)]
    for (x, y, z) in tops:
        # hang toward the side of the building the chimney is nearest, i.e. the shortest fall
        best = None
        for a in np.linspace(0, 2 * math.pi, 16, endpoint=False):
            d = (math.cos(a), math.sin(a))
            e = eave_toward(F, x + d[0] * 0.5, z + d[1] * 0.5, d, y)
            L = math.hypot(e[0] - x, e[2] - z)
            if L > 1.5 and (best is None or L < best[0]):
                best = (L, e)
        if best is None:
            continue
        L, e = best
        drape(sheet, F, (x, y + 0.05, z), e, 0.5, float(np.clip(0.6 * L, 1.8, 3.6)), rng,
              lip=rng.uniform(1.2, 2.2))
        placed.append(("drape", np.array([e[0], e[2]])))

    # eave curtain on a long hull edge away from the tents
    edges = []
    for k in range(len(poly)):
        a, b = np.array(poly[k]), np.array(poly[(k + 1) % len(poly)])
        L = np.linalg.norm(b - a)
        if L < 3.0:
            continue
        mid = (a + b) / 2
        if any(np.linalg.norm(mid - p[1]) < 2.5 for p in placed):
            continue
        edges.append((L, a, b))
    edges.sort(key=lambda e: -e[0])
    for L, a, b in edges[: (2 if kind == "townhall" else 1)]:
        dvec = (b - a) / L
        out = np.array([dvec[1], -dvec[0]])
        if (a + b) / 2 @ out < cen @ out:
            out = -out
        span = min(L * 0.6, rng.uniform(2.6, 3.6))
        s0 = rng.uniform(0.15, 0.85 - span / L) if L > span / 0.7 else 0.2
        q0, q1 = a + dvec * L * s0, a + dvec * (L * s0 + span)
        # eave height: highest roof cell just inside this stretch
        hs = [float(F.h(*(q0 + (q1 - q0) * t - out * 0.3))) for t in np.linspace(0, 1, 7)]
        ey = min(hs) - 0.15
        if ey < F.ground + 2.2:
            continue
        curtain(sheet, F, (q0[0], ey, q0[1]), (q1[0], ey, q1[1]), rng.uniform(0.9, 1.6), out, rng)
        placed.append(("curtain", (q0 + q1) / 2))
    return sheet, placed, F, poly
