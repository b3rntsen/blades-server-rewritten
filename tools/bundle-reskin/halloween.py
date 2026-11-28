#!/usr/bin/env python3
"""Halloween: a per-material autumn palette with pumpkins strewn on the ground.

Same shape as `easter.py`, and for the same measured reasons: one duotone
flattens a scene to a single hue, so each material family gets its own ramp; and
a picture cannot sit on tiled or atlas-packed architecture, so the only pictorial
element -- the pumpkins -- goes on ground textures, where a repeating pattern
reads as a patch rather than a smear.

Bethesda's own Halloween was decoration PROPS (see docs/seasonal-decorations.md),
which we already hold and can grant. This is the town skin underneath them.
"""
import random
import zlib

from PIL import Image, ImageDraw, ImageEnhance, ImageOps

PALETTES = {
    "grass": ((16, 14, 10), (62, 48, 20), (124, 98, 40)),
    "roof": ((26, 12, 28), (92, 36, 60), (188, 92, 96)),
    "stone": ((28, 22, 30), (96, 80, 92), (206, 186, 176)),
    "brick": ((34, 18, 16), (118, 58, 32), (214, 128, 60)),
    "wood": ((26, 16, 10), (92, 54, 26), (176, 116, 58)),
    "dirt": ((24, 18, 14), (86, 62, 40), (170, 132, 88)),
    "metal": ((22, 22, 28), (84, 86, 96), (186, 190, 200)),
    "foliage": ((18, 10, 6), (84, 36, 12), (158, 74, 20)),
    "fabric": ((30, 16, 8), (126, 64, 20), (226, 146, 48)),
    "default": ((26, 20, 24), (96, 72, 72), (198, 164, 140)),
}

CLASSES = (
    ("grass", ("lawn", "grass", "moss")),
    ("foliage", ("tree", "leaf", "leaves", "branch", "bush", "forest", "fern",
                 "plant", "flower", "ivy", "vine")),
    ("roof", ("roof", "tile", "shingle")),
    ("brick", ("brick", "plaster", "wall")),
    ("stone", ("stone", "rock", "cliff", "marble", "granite", "sculpt",
               "statue", "road", "cobble")),
    ("wood", ("wood", "plank", "log", "crosswood", "timber")),
    ("metal", ("iron", "copper", "metal", "chain", "steel", "gold")),
    ("dirt", ("dirt", "mud", "ground", "sand", "decal", "soil")),
    ("fabric", ("fabric", "rug", "cloth", "banner", "rope", "canvas")),
)

STRENGTH = 0.82
GROUND_FAMILIES = {"grass", "dirt"}

BODY = (236, 126, 26)
RIB = (196, 92, 14)
CUT = (14, 8, 4)
STEM = (76, 96, 40)


def classify(name):
    low = name.lower()
    for family, keys in CLASSES:
        if any(k in low for k in keys):
            return family
    return "default"


def _ramp(shadow, mid, high):
    out = []
    for i in range(256):
        t = i / 255.0
        if t < 0.5:
            u = t / 0.5
            c = [shadow[k] + (mid[k] - shadow[k]) * u for k in range(3)]
        else:
            u = (t - 0.5) / 0.5
            c = [mid[k] + (high[k] - mid[k]) * u for k in range(3)]
        out.append(tuple(int(max(0, min(255, v))) for v in c))
    return out


_RAMPS = {k: _ramp(*v) for k, v in PALETTES.items()}


def _pumpkin(d, cx, cy, r):
    d.rectangle([cx - r * 0.12, cy - r * 1.18, cx + r * 0.12, cy - r * 0.62],
                fill=STEM)
    for dx, w in ((0, 1.0), (-0.40, 0.70), (0.40, 0.70)):
        d.ellipse([cx + dx * r - r * w, cy - r * 0.74,
                   cx + dx * r + r * w, cy + r * 0.86], fill=BODY)
    d.ellipse([cx - r * 0.52, cy - r * 0.60, cx + r * 0.52, cy + r * 0.74],
              outline=RIB, width=max(1, int(r * 0.09)))
    # a face, small but high-contrast, so it survives the mip chain
    d.polygon([(cx - r * 0.58, cy - r * 0.22), (cx - r * 0.16, cy + r * 0.04),
               (cx - r * 0.60, cy + r * 0.14)], fill=CUT)
    d.polygon([(cx + r * 0.58, cy - r * 0.22), (cx + r * 0.16, cy + r * 0.04),
               (cx + r * 0.60, cy + r * 0.14)], fill=CUT)
    d.chord([cx - r * 0.60, cy + r * 0.06, cx + r * 0.60, cy + r * 0.72],
            10, 170, fill=CUT)


def add_pumpkins(img, count=18, seed=5):
    size = img.size[0]
    layer = img.copy()
    d = ImageDraw.Draw(layer)
    rnd = random.Random(seed)
    for _ in range(count):
        cx, cy = rnd.randrange(size), rnd.randrange(size)
        r = rnd.randint(int(size * 0.030), int(size * 0.048))
        # every nine offsets, so the tile still repeats without a seam
        for ox in (-size, 0, size):
            for oy in (-size, 0, size):
                _pumpkin(d, cx + ox, cy + oy, r)
    return layer


def grade(img, name):
    family = classify(name)
    rgb = img.convert("RGB")
    lum = ImageOps.grayscale(rgb)
    lut = _RAMPS[family]
    duo = Image.merge("RGB", [lum.point([lut[i][ch] for i in range(256)])
                              for ch in range(3)])
    out = Image.blend(rgb, duo, STRENGTH)
    out = ImageEnhance.Color(out).enhance(1.25)
    # darker overall than Easter: this is a night-ish autumn, not a bright day
    out = ImageEnhance.Brightness(out).enhance(0.92)
    if family in GROUND_FAMILIES and out.size[0] == out.size[1] >= 256:
                # zlib.crc32, not hash(): Python salts string hashing per process, so
        # hash(name) gave this texture a different scatter on every run and the
        # same pack could never be rebuilt byte-for-byte. crc32 is stable across
        # processes and versions, which is what makes a rebuild verifiable.
        out = add_pumpkins(out, seed=zlib.crc32(name.encode()) % 9973)
    return out
