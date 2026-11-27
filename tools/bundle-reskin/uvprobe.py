"""A texture that makes UV tiling countable.

Each tile is four bright quadrants inside a thick black border. Counting the
black grid lines across a surface gives the tiling factor directly, and the
quadrant colours are asymmetric so the orientation and any mirroring are visible
too. Without this the pumpkin size is guesswork -- which is exactly what made
the first authored texture smear into an orange wash.
"""
from PIL import Image, ImageDraw


def draw(size=512):
    img = Image.new("RGB", (size, size), (0, 0, 0))
    d = ImageDraw.Draw(img)
    b = size // 12                      # border thickness
    h = size // 2
    quads = [((b, b, h, h), (255, 40, 40)),        # TL red
             ((h, b, size - b, h), (40, 255, 40)),  # TR green
             ((b, h, h, size - b), (60, 110, 255)), # BL blue
             ((h, h, size - b, size - b), (255, 255, 255))]  # BR white
    for box, col in quads:
        d.rectangle(box, fill=col)
    return img


if __name__ == "__main__":
    t = draw()
    sheet = Image.new("RGB", (1024, 1024))
    for x in (0, 512):
        for y in (0, 512):
            sheet.paste(t, (x, y))
    sheet.save("uvprobe_preview.png")
    print("wrote uvprobe_preview.png")
