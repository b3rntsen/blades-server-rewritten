"""A seamless stone ground with jack-o'-lanterns strewn across it.

The ground texture tiles many times across the visible road, so one big pumpkin
per tile would smear into an orange wash at camera distance -- what reads as
Halloween is several small carved pumpkins on stone that still looks like stone.
Every pumpkin that crosses an edge is drawn again on the opposite edge, so the
tile repeats without a seam.
"""
from PIL import Image, ImageDraw
import random

STONE = (104, 100, 92)


def _pumpkin(d, cx, cy, r):
    body = (228, 122, 28)
    dark = (20, 10, 5)
    d.ellipse([cx - r * 0.30, cy - r * 1.25, cx + r * 0.30, cy - r * 0.55],
              fill=(86, 116, 46))
    for dx, w in ((0, 1.0), (-0.42, 0.72), (0.42, 0.72)):
        d.ellipse([cx + dx * r - r * w, cy - r * 0.72,
                   cx + dx * r + r * w, cy + r * 0.92], fill=body)
    # face
    d.polygon([(cx - r * 0.62, cy - r * 0.26), (cx - r * 0.18, cy),
               (cx - r * 0.66, cy + r * 0.10)], fill=dark)
    d.polygon([(cx + r * 0.62, cy - r * 0.26), (cx + r * 0.18, cy),
               (cx + r * 0.66, cy + r * 0.10)], fill=dark)
    d.chord([cx - r * 0.66, cy + r * 0.04, cx + r * 0.66, cy + r * 0.76],
            10, 170, fill=dark)


def draw(size=512, count=26, seed=7):
    img = Image.new("RGB", (size, size), STONE)
    d = ImageDraw.Draw(img)
    rnd = random.Random(seed)
    # cobble mottling, so the stone does not read as flat paint
    for _ in range(900):
        x, y = rnd.randrange(size), rnd.randrange(size)
        r = rnd.randint(6, 26)
        v = rnd.randint(-22, 18)
        fill = tuple(max(0, min(255, c + v)) for c in STONE)
        for ox in (-size, 0, size):
            for oy in (-size, 0, size):
                d.ellipse([x + ox - r, y + oy - r, x + ox + r, y + oy + r],
                          fill=fill)
    spots = [(rnd.randrange(size), rnd.randrange(size),
              rnd.randint(int(size * 0.035), int(size * 0.055)))
             for _ in range(count)]
    # Draw each pumpkin in all nine positions; only the wrapped copies that
    # actually overlap the tile land inside it, which is what makes it seamless.
    for cx, cy, r in spots:
        for ox in (-size, 0, size):
            for oy in (-size, 0, size):
                _pumpkin(d, cx + ox, cy + oy, r)
    return img


if __name__ == "__main__":
    t = draw()
    sheet = Image.new("RGB", (1024, 1024))
    for x in (0, 512):
        for y in (0, 512):
            sheet.paste(t, (x, y))
    sheet.save("ground_tiled_preview.png")
    print("wrote ground_tiled_preview.png (2x2 tiling, check the seams)")
