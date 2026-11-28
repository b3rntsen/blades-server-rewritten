#!/usr/bin/env python3
"""Winter: cold blue-white per material, with snow settled on the ground.

Same shape and same constraints as `easter.py` and `halloween.py`. The one thing
that differs is how the ground is treated: snow is not a scattered motif but a
covering, so instead of drawing objects the ground families are lifted towards
white with a little drift noise. Drawn the other way -- discrete snowflakes on a
tiling texture -- it reads as dirt, not snow.
"""
import random
import zlib

from PIL import Image, ImageDraw, ImageEnhance, ImageOps

PALETTES = {
    "grass": ((58, 72, 96), (162, 186, 210), (244, 250, 255)),
    "roof": ((40, 54, 82), (128, 158, 196), (236, 244, 255)),
    "stone": ((54, 64, 84), (146, 164, 190), (240, 246, 255)),
    "brick": ((60, 66, 84), (150, 158, 178), (238, 242, 250)),
    "wood": ((48, 44, 48), (118, 104, 100), (206, 196, 190)),
    "dirt": ((62, 72, 90), (156, 174, 196), (242, 248, 255)),
    "metal": ((44, 56, 76), (140, 164, 196), (240, 248, 255)),
    "foliage": ((40, 58, 62), (118, 154, 152), (226, 242, 244)),
    "fabric": ((52, 60, 88), (136, 152, 198), (236, 240, 255)),
    "default": ((54, 64, 86), (148, 164, 192), (240, 246, 255)),
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

STRENGTH = 0.86
GROUND_FAMILIES = {"grass", "dirt"}


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


def settle_snow(img, seed=3):
    """Lift the ground towards white, with soft drifts.

    Wrapped on all nine offsets like every other ground motif here, so the tile
    still repeats seamlessly -- a visible seam across a town square is the one
    artefact people notice immediately.
    """
    size = img.size[0]
    layer = img.copy()
    d = ImageDraw.Draw(layer, "RGBA")
    rnd = random.Random(seed)
    for _ in range(140):
        cx, cy = rnd.randrange(size), rnd.randrange(size)
        r = rnd.randint(int(size * 0.04), int(size * 0.13))
        a = rnd.randint(70, 150)
        for ox in (-size, 0, size):
            for oy in (-size, 0, size):
                d.ellipse([cx + ox - r, cy + oy - r, cx + ox + r, cy + oy + r],
                          fill=(248, 252, 255, a))
    return Image.blend(img, layer.convert("RGB"), 0.9)


def grade(img, name):
    family = classify(name)
    rgb = img.convert("RGB")
    lum = ImageOps.grayscale(rgb)
    lut = _RAMPS[family]
    duo = Image.merge("RGB", [lum.point([lut[i][ch] for i in range(256)])
                              for ch in range(3)])
    out = Image.blend(rgb, duo, STRENGTH)
    # Desaturate rather than boost: snow light is cold and low-chroma, and the
    # per-family ramps already carry what little colour there should be.
    out = ImageEnhance.Color(out).enhance(0.75)
    out = ImageEnhance.Brightness(out).enhance(1.12)
    if family in GROUND_FAMILIES and out.size[0] == out.size[1] >= 256:
                # zlib.crc32, not hash(): Python salts string hashing per process, so
        # hash(name) gave this texture a different scatter on every run and the
        # same pack could never be rebuilt byte-for-byte. crc32 is stable across
        # processes and versions, which is what makes a rebuild verifiable.
        out = settle_snow(out, seed=zlib.crc32(name.encode()) % 9973)
    return out
