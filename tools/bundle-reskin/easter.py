#!/usr/bin/env python3
"""An Easter re-skin: spring palette per material, plus an egg hunt on the ground.

Two things learned the hard way shape this. The surfaces are tiled and
atlas-packed, so no picture can be painted across a building -- but a *pattern*
that repeats is fine, which is why the eggs go on ground textures and read as an
egg hunt rather than as a smear. And a single duotone flattens everything to one
hue; a season that should feel bright needs the palette to vary by material, so
stone, roofs, grass and wood are each given their own spring ramp keyed off the
texture's own name.

Detail is preserved by grading the ORIGINAL art rather than replacing it: the
luminance drives the ramp, and the result is blended back over the source.
"""
import random
import zlib
import re

from PIL import Image, ImageDraw, ImageEnhance, ImageOps

# shadow -> midtone -> highlight, per material family
PALETTES = {
    "grass": ((26, 74, 30), (86, 176, 62), (190, 236, 120)),
    "roof": ((60, 40, 96), (150, 110, 205), (236, 206, 255)),
    "stone": ((92, 78, 110), (206, 186, 214), (255, 248, 252)),
    "brick": ((120, 70, 96), (234, 160, 186), (255, 228, 238)),
    "wood": ((92, 58, 34), (196, 138, 84), (250, 218, 170)),
    "dirt": ((96, 74, 58), (188, 156, 124), (246, 224, 196)),
    "metal": ((70, 84, 110), (168, 190, 220), (244, 250, 255)),
    "foliage": ((30, 80, 40), (104, 190, 78), (206, 244, 140)),
    "fabric": ((120, 96, 40), (244, 206, 96), (255, 246, 196)),
    "default": ((84, 74, 104), (198, 182, 220), (255, 246, 250)),
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

EGG_COLOURS = [(255, 126, 168), (255, 214, 92), (124, 214, 255), (170, 132, 255),
               (140, 230, 140), (255, 168, 104), (255, 246, 250)]

STRENGTH = 0.80
GROUND_FAMILIES = {"grass", "dirt"}


def classify(name):
    low = name.lower()
    for family, keys in CLASSES:
        if any(k in low for k in keys):
            return family
    return "default"


def _ramp(shadow, mid, high):
    lut = []
    for i in range(256):
        t = i / 255.0
        if t < 0.5:
            u = t / 0.5
            lut.append(tuple(shadow[k] + (mid[k] - shadow[k]) * u for k in range(3)))
        else:
            u = (t - 0.5) / 0.5
            lut.append(tuple(mid[k] + (high[k] - mid[k]) * u for k in range(3)))
    return [tuple(int(max(0, min(255, v))) for v in c) for c in lut]


_RAMPS = {k: _ramp(*v) for k, v in PALETTES.items()}


def _egg(d, cx, cy, rx, ry, colour, seed):
    rnd = random.Random(seed)
    d.ellipse([cx - rx, cy - ry, cx + rx, cy + ry], fill=colour)
    # a couple of bands, so it reads as a painted egg and not a blob
    band = tuple(max(0, min(255, int(c * 0.62) + 40)) for c in colour)
    for frac in (0.32, 0.62):
        y = cy - ry + 2 * ry * frac
        h = max(1.0, ry * 0.16)
        d.ellipse([cx - rx, y - h, cx + rx, y + h], fill=band)
    d.ellipse([cx - rx * 0.44, cy - ry * 0.58, cx - rx * 0.06, cy - ry * 0.2],
              fill=(255, 255, 255))
    _ = rnd


def add_eggs(img, count=14, seed=11):
    """Scatter wrapped eggs so the tile still repeats seamlessly."""
    size = img.size[0]
    layer = img.copy()
    d = ImageDraw.Draw(layer)
    rnd = random.Random(seed)
    for i in range(count):
        cx, cy = rnd.randrange(size), rnd.randrange(size)
        rx = rnd.randint(int(size * 0.026), int(size * 0.042))
        ry = int(rx * 1.34)
        colour = EGG_COLOURS[i % len(EGG_COLOURS)]
        for ox in (-size, 0, size):
            for oy in (-size, 0, size):
                _egg(d, cx + ox, cy + oy, rx, ry, colour, seed + i)
    return layer


def grade(img, name):
    family = classify(name)
    rgb = img.convert("RGB")
    lum = ImageOps.grayscale(rgb)
    lut = _RAMPS[family]
    duo = Image.merge("RGB", [lum.point([lut[i][ch] for i in range(256)])
                              for ch in range(3)])
    out = Image.blend(rgb, duo, STRENGTH)
    out = ImageEnhance.Color(out).enhance(1.45)
    out = ImageEnhance.Brightness(out).enhance(1.10)
    if family in GROUND_FAMILIES and out.size[0] == out.size[1] >= 256:
                # zlib.crc32, not hash(): Python salts string hashing per process, so
        # hash(name) gave this texture a different scatter on every run and the
        # same pack could never be rebuilt byte-for-byte. crc32 is stable across
        # processes and versions, which is what makes a rebuild verifiable.
        out = add_eggs(out, seed=zlib.crc32(name.encode()) % 9973)
    return out


if __name__ == "__main__":
    base = Image.new("RGB", (512, 512), (120, 120, 120))
    d = ImageDraw.Draw(base)
    rnd = random.Random(3)
    for _ in range(600):
        x, y = rnd.randrange(512), rnd.randrange(512)
        r = rnd.randint(8, 30)
        v = rnd.randint(-30, 30)
        d.ellipse([x - r, y - r, x + r, y + r], fill=(120 + v,) * 3)
    sheet = Image.new("RGB", (1024, 1024))
    for i, nm in enumerate(("Town_Lawn_alb", "Town_RoofTiles_alb",
                            "Town_BrickWalls_01_alb", "Town_WoodPlanks_alb")):
        sheet.paste(grade(base, nm).resize((512, 512)),
                    ((i % 2) * 512, (i // 2) * 512))
    sheet.save("easter_preview.png")
    print("wrote easter_preview.png  (lawn / roof / brick / wood)")
