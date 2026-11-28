"""Tiny z-buffer rasteriser: opaque building, alpha-blended webs, perspective camera."""
import math

import numpy as np
from PIL import Image


def look(eye, target, up=(0, 1, 0)):
    eye, target, up = map(lambda a: np.asarray(a, float), (eye, target, up))
    f = target - eye
    f /= np.linalg.norm(f)
    r = np.cross(f, up)
    r /= np.linalg.norm(r)
    u = np.cross(r, f)
    return eye, r, u, f


def raster(tris, cam, W, H, fov=50, shade=None, zbuf=None, color=None, alpha=None, img=None):
    eye, r, u, f = cam
    rel = tris - eye
    x, y, z = rel @ r, rel @ u, rel @ f
    k = 1 / math.tan(math.radians(fov) / 2)
    sx = (x / np.maximum(z, 1e-3) * k * 0.5 + 0.5) * W
    sy = (0.5 - y / np.maximum(z, 1e-3) * k * 0.5 * W / H) * H
    for t in range(len(tris)):
        if (z[t] < 0.2).any():
            continue
        xs, ys, zs = sx[t], sy[t], z[t]
        x0, x1 = max(int(xs.min()), 0), min(int(xs.max()) + 1, W - 1)
        y0, y1 = max(int(ys.min()), 0), min(int(ys.max()) + 1, H - 1)
        if x0 > x1 or y0 > y1:
            continue
        gx, gy = np.meshgrid(np.arange(x0, x1 + 1) + 0.5, np.arange(y0, y1 + 1) + 0.5)
        d = (ys[1] - ys[2]) * (xs[0] - xs[2]) + (xs[2] - xs[1]) * (ys[0] - ys[2])
        if abs(d) < 1e-9:
            continue
        l1 = ((ys[1] - ys[2]) * (gx - xs[2]) + (xs[2] - xs[1]) * (gy - ys[2])) / d
        l2 = ((ys[2] - ys[0]) * (gx - xs[2]) + (xs[0] - xs[2]) * (gy - ys[2])) / d
        l3 = 1 - l1 - l2
        m = (l1 >= 0) & (l2 >= 0) & (l3 >= 0)
        if not m.any():
            continue
        zz = l1 * zs[0] + l2 * zs[1] + l3 * zs[2]
        sub = zbuf[y0:y1 + 1, x0:x1 + 1]
        m &= zz < sub
        if alpha is None:
            sub[m] = zz[m]
            img[y0:y1 + 1, x0:x1 + 1][m] = color * shade[t]
        else:
            px = img[y0:y1 + 1, x0:x1 + 1]
            px[m] = px[m] * (1 - alpha) + color * shade[t] * alpha


def render(parts, webs, out, views, W=480, H=360):
    L = np.array([0.4, 0.8, 0.3])
    L /= np.linalg.norm(L)
    tiles = []
    for eye, target in views:
        cam = look(eye, target)
        img = np.zeros((H, W, 3)) + np.array([0.12, 0.12, 0.25])
        zb = np.full((H, W), 1e9)
        tri = np.concatenate([v[f] for _, _, v, f in parts])
        n = np.cross(tri[:, 1] - tri[:, 0], tri[:, 2] - tri[:, 0])
        n /= np.linalg.norm(n, axis=1)[:, None] + 1e-9
        raster(tri, cam, W, H, shade=0.35 + 0.65 * np.abs(n @ L), zbuf=zb,
               color=np.array([0.6, 0.55, 0.5]), img=img)
        for wv, wi in webs:
            wt = wv[wi]
            raster(wt, cam, W, H, shade=np.ones(len(wt)), zbuf=zb,
                   color=np.array([0.9, 0.95, 1.0]), alpha=0.45, img=img)
        tiles.append((np.clip(img, 0, 1) * 255).astype(np.uint8))
    Image.fromarray(np.concatenate(tiles, axis=1)).save(out)
