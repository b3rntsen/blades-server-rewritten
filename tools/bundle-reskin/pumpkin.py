"""Draw a jack-o'-lantern at 512x512. It lands on a tiling wall texture, so it
is composed to repeat cleanly: the pumpkin sits fully inside the tile with a
dark border, rather than bleeding across the seam."""
from PIL import Image, ImageDraw

def draw(size=512):
    img = Image.new("RGB", (size, size), (26, 20, 16))
    d = ImageDraw.Draw(img)
    s = size / 512

    def box(x0, y0, x1, y1):
        return [x0 * s, y0 * s, x1 * s, y1 * s]

    # stem
    d.rectangle(box(238, 96, 274, 150), fill=(74, 102, 40))
    d.ellipse(box(228, 84, 284, 118), fill=(92, 124, 50))
    # body: overlapping lobes give the ribbed silhouette
    for cx, w in ((256, 168), (196, 128), (316, 128), (150, 92), (362, 92)):
        d.ellipse(box(cx - w / 2, 150, cx + w / 2, 420), fill=(214, 108, 22))
    d.ellipse(box(150, 160, 362, 412), fill=(236, 126, 26))
    # rib shading
    for cx in (196, 256, 316):
        d.ellipse(box(cx - 52, 168, cx + 52, 404), outline=(198, 96, 18), width=int(4 * s) or 1)
    # face
    d.polygon([(200 * s, 236 * s), (248 * s, 268 * s), (196 * s, 280 * s)], fill=(24, 12, 6))
    d.polygon([(312 * s, 236 * s), (264 * s, 268 * s), (316 * s, 280 * s)], fill=(24, 12, 6))
    d.polygon([(232 * s, 292 * s), (256 * s, 268 * s), (280 * s, 292 * s),
               (268 * s, 312 * s), (244 * s, 312 * s)], fill=(24, 12, 6))
    # grin with teeth
    d.chord(box(168, 268, 344, 386), 15, 165, fill=(24, 12, 6))
    for tx in (196, 232, 268, 304):
        d.polygon([(tx * s, 330 * s), ((tx + 22) * s, 330 * s), ((tx + 11) * s, 356 * s)],
                  fill=(236, 126, 26))
    return img

if __name__ == "__main__":
    draw().save("pumpkin_src.png")
    print("wrote pumpkin_src.png")
