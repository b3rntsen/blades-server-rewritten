#!/usr/bin/env python3
"""A cobweb texture for the ivy that already grows on the town's walls.

Retail hung webs on shop fronts. We cannot add objects to a scene we can only
patch -- but the town already has 54 ivy patches clinging to exactly those
walls, all sharing one material. Re-texturing that material turns every one of
them into webbing, in the places webs belong, with no authoring at all.

**The texture must carry alpha.** An earlier version of this file asserted the
opposite -- "ASTC_RGB (no alpha), so the shape comes from the mesh" -- and drew
the web on an opaque pale sheet. That is wrong twice over. The ivy mesh is a
flat card: every leaf edge in the stock texture comes from its alpha, whose
range is 0..255, and the material is a cutout (`_Mode` 1, `_ALPHATEST_ON`,
`_Cutoff` 0.411). Shipping an opaque texture through a cutout shader means
nothing is ever cut, so all 54 cards rendered as full grey rectangles draped on
the buildings, which is what a player reported as "a little unrealistic".

Alpha survives the round-trip: ASTC stores four channels whatever the format
enum says it was imported as, and writing a known 0/255 pattern back through
UnityPy at `packer="original"` reads back as exactly 0/255.

`_Cutoff` is a hard test, so there is no partial transparency to be had -- a
strand is either drawn or not. The web is therefore rendered at 4x and
downsampled with the alpha thresholded rather than blurred, which keeps the
strands from thinning into nothing while leaving the edge binary.

Better still, do not draw one at all. `clutter_spiderwebs_sab` holds Bethesda's
own webs -- 28 `SpiderWeb_*` meshes and two 1024x1024 textures whose alpha is a
real 0..255 cutout. `--real` lifts that texture onto the ivy card, so the town
gets the same web art retail used rather than an approximation of it.

The moss was suggested as the alpha donor and cannot be one: all four moss
materials (`MossTile_01_alb`, `RockWall_Mossy_01/02_alb`, `Boulders_Mossy_01_alb`)
are `_Mode` 0 opaque with no `_ALPHATEST_ON`, and their albedos measure
255..255 -- there is no cutout in them to borrow. The ivy is the only carrier in
`landscape_sab` that is both a cutout material and a card, which is why it stays
the target.

    ./cobweb.py                             # writes cobweb_preview.png
    ./cobweb.py <src bundle> <dst>          # drawn web onto Foliage_Ivy_alb
    ./cobweb.py <src> <dst> --real <webs>   # Bethesda's web instead
"""
import math
import os
import random
import sys

from PIL import Image, ImageDraw

#: Near-white, and applied to every texel including the cut-away ones. A
#: transparent pixel still has a colour, and bilinear filtering blends it into
#: the strand edges -- leave it black and every strand gets a dark fringe.
STRAND = (246, 249, 255)

#: A texel is kept when this share of it is covered (0-255 of full coverage).
#: A threshold, not a blend: the material's 0.411 cutoff is a hard test, so
#: partial alpha has nowhere to go.
COVERAGE = 70

#: The web is drawn this many times larger and then reduced. At 1x a strand is
#: one pixel, which is below what a mip chain can carry -- the webs then fade
#: out with distance rather than staying put.
SUPERSAMPLE = 4


def draw(size=128):
    """Return the RGBA cobweb. Transparent everywhere but the strands."""
    s = size * SUPERSAMPLE
    mask = Image.new("L", (s, s), 0)
    d = ImageDraw.Draw(mask)
    rnd = random.Random(5)

    cx = cy = s / 2
    spokes = 12
    # One texel wide once reduced. Thinner than this and the downsample leaves
    # a dashed line wherever a strand runs diagonally, which reads as a broken
    # web rather than a fine one.
    width = SUPERSAMPLE

    for i in range(spokes):
        a = 2 * math.pi * i / spokes
        d.line([cx, cy, cx + math.cos(a) * s, cy + math.sin(a) * s],
               fill=255, width=width)

    # Rings sag between the spokes, which is what stops it reading as a dartboard.
    for ring in range(1, 7):
        rr = s * ring / 13.0
        pts = []
        for i in range(spokes * 4 + 1):
            a = 2 * math.pi * i / (spokes * 4)
            sag = rr * (0.90 + 0.06 * math.sin(a * spokes / 2.0))
            pts.append((cx + math.cos(a) * sag, cy + math.sin(a) * sag))
        d.line(pts, fill=255, width=width)

    # A few loose strands trailing off, which is what sells it at a distance.
    for _ in range(14):
        x0, y0 = rnd.randrange(s), rnd.randrange(s)
        d.line([x0, y0,
                x0 + rnd.randint(-28, 28) * SUPERSAMPLE,
                y0 + rnd.randint(-28, 28) * SUPERSAMPLE],
               fill=255, width=width)

    # BOX is an area average, so a texel's value IS its coverage; LANCZOS rings
    # and undershoots, which is what dashed the diagonals on the first attempt.
    alpha = mask.resize((size, size), Image.BOX).point(
        lambda v: 255 if v >= COVERAGE else 0)
    img = Image.new("RGBA", (size, size), STRAND + (0,))
    img.putalpha(alpha)
    return img


#: Bethesda's own web. `Spiderweb_01` is the corner web with a hanging strand;
#: `SpiderWeb_02` is sparser (1.6% of texels survive the cutoff against 5.8%),
#: which on a card this size reads as a few loose threads rather than a web.
REAL_WEB = "Spiderweb_01"

#: The ivy card is 128px, and a 1024px web reduced that far loses its strands to
#: the cutoff. Raising the card to this costs ~40 KB of ASTC and keeps them.
REAL_WEB_SIZE = 256


def real_web(webs_bundle, size=REAL_WEB_SIZE, name=REAL_WEB):
    """Lift Bethesda's web texture out of clutter_spiderwebs_sab."""
    import UnityPy
    from PIL import Image

    for obj in UnityPy.load(webs_bundle).objects:
        if obj.type.name != "Texture2D":
            continue
        tex = obj.read()
        if tex.m_Name != name:
            continue
        img = tex.image.convert("RGBA")
        rgb = img.convert("RGB").resize((size, size), Image.LANCZOS)
        # Reduce the alpha by area coverage and re-threshold, for the same
        # reason the drawn web does: a hard cutoff plus a soft downscale eats
        # thin strands, and this web is nearly all thin strands.
        alpha = img.split()[3].resize((size, size), Image.BOX).point(
            lambda v: 255 if v >= COVERAGE else 0)
        out = rgb.convert("RGBA")
        out.putalpha(alpha)
        return out
    raise SystemExit("cobweb: %s not found in %s" % (name, webs_bundle))


def patch(src, dst, texture="Foliage_Ivy_alb", webs_bundle=None):
    """Replace `texture` in `src` with the cobweb, writing `dst`."""
    import UnityPy

    env = UnityPy.load(src)
    hit = None
    for obj in env.objects:
        if obj.type.name != "Texture2D":
            continue
        tex = obj.read()
        if tex.m_Name != texture:
            continue
        art = real_web(webs_bundle) if webs_bundle else draw(tex.m_Width)
        tex.image = art
        tex.save()
        hit = art.size[0]
    if hit is None:
        raise SystemExit(
            "cobweb: %s not found in %s -- refusing to write a bundle that "
            "would ship unchanged" % (texture, src))

    data = env.file.save(packer="original")
    with open(dst, "wb") as fh:
        fh.write(data)

    # Read it back. The whole defect this file exists to fix was an opaque
    # texture reaching a cutout shader, and that is invisible without looking.
    lo, hi = _alpha_range(dst, texture)
    if (lo, hi) != (0, 255):
        raise SystemExit(
            "cobweb: %s came back with alpha %d..%d, not 0..255 -- the cutout "
            "would not cut and every card would render as a rectangle"
            % (texture, lo, hi))
    print("  %s: %s at %dpx, alpha %d..%d, %d bytes (was %d)"
          % (os.path.basename(dst), texture, hit, lo, hi, len(data),
             os.path.getsize(src)))


def _alpha_range(path, texture):
    import UnityPy

    for obj in UnityPy.load(path).objects:
        if obj.type.name != "Texture2D":
            continue
        tex = obj.read()
        if tex.m_Name == texture:
            return tex.image.convert("RGBA").split()[3].getextrema()
    return (-1, -1)


if __name__ == "__main__":
    if len(sys.argv) >= 3:
        webs = None
        if "--real" in sys.argv:
            webs = sys.argv[sys.argv.index("--real") + 1]
        patch(sys.argv[1], sys.argv[2], webs_bundle=webs)
    else:
        t = draw()
        sheet = Image.new("RGBA", (256, 256), (0, 0, 0, 0))
        for x in (0, 128):
            for y in (0, 128):
                sheet.paste(t, (x, y))
        sheet.save("cobweb_preview.png")
        print("wrote cobweb_preview.png")
