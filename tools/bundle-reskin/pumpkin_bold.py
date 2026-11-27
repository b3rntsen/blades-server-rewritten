"""A jack-o'-lantern sized to fill its tile.

The UV probe showed this wall tiles only about six times across, so the pumpkin
should occupy nearly the whole tile rather than sit in the middle of it -- the
first version left a wide dark margin, and at camera distance the margin and the
body averaged into a plain orange wash. Contrast does the rest: near-black
ground, one saturated orange, black cut-outs.
"""
from PIL import Image, ImageDraw

GROUND = (14, 11, 9)
BODY = (242, 126, 18)
RIB = (196, 92, 10)
CUT = (10, 6, 3)


def draw(size=512):
    img = Image.new("RGB", (size, size), GROUND)
    d = ImageDraw.Draw(img)
    s = size / 512.0

    def B(x0, y0, x1, y1):
        return [x0 * s, y0 * s, x1 * s, y1 * s]

    # stem
    d.rectangle(B(236, 40, 276, 104), fill=(70, 100, 34))
    d.ellipse(B(222, 24, 290, 74), fill=(92, 126, 44))
    # body: lobes out to the tile edge
    for cx, w in ((256, 250), (128, 168), (384, 168), (60, 112), (452, 112)):
        d.ellipse(B(cx - w / 2, 96, cx + w / 2, 492), fill=BODY)
    d.ellipse(B(24, 110, 488, 486), fill=BODY)
    for cx in (128, 256, 384):
        d.ellipse(B(cx - 78, 120, cx + 78, 478), outline=RIB, width=max(1, int(5 * s)))
    # face, large enough to survive the mip chain
    d.polygon([(118 * s, 236 * s), (222 * s, 300 * s), (110 * s, 324 * s)], fill=CUT)
    d.polygon([(394 * s, 236 * s), (290 * s, 300 * s), (402 * s, 324 * s)], fill=CUT)
    d.polygon([(214 * s, 342 * s), (256 * s, 288 * s), (298 * s, 342 * s),
               (276 * s, 382 * s), (236 * s, 382 * s)], fill=CUT)
    d.chord(B(84, 320, 428, 470), 12, 168, fill=CUT)
    for tx in (120, 190, 260, 330):
        d.polygon([(tx * s, 392 * s), ((tx + 44) * s, 392 * s),
                   ((tx + 22) * s, 442 * s)], fill=BODY)
    return img


if __name__ == "__main__":
    draw().save("pumpkin_bold_preview.png")
    print("wrote pumpkin_bold_preview.png")
