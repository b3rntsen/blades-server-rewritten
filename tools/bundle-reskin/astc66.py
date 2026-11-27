"""Write an ASTC_RGB_6x6 texture with a full mip chain, using void-extent blocks.

An ASTC "void extent" block encodes one constant colour in its 16 bytes, so a
texture can be authored with no encoder at all: each 6x6 pixel tile becomes one
solid block. At 512x512 that is an 86x86 grid -- coarse, but more than enough to
draw a recognisable shape, and every byte is exact, which is what matters when
the result has to be spliced into a .resS at a fixed offset and size.

Layout (ASTC spec, bits are LSB-first):
  [8:0]   = 0b111111100  void-extent marker
  [9]     = 0           LDR
  [11:10] = 0b11        reserved, must be 1
  [63:12] = all ones    "no extent recorded", the safe constant-colour form
  [127:64]= R,G,B,A as UNORM16
"""
import struct

BLOCK = 6
_LOW = 0xFFFFFFFFFFFFFDFC


def void_extent(rgb):
    r, g, b = rgb
    return struct.pack("<QHHHH", _LOW, r * 257, g * 257, b * 257, 0xFFFF)


def _blocks(dim):
    return (dim + BLOCK - 1) // BLOCK


def encode(img, width=512, height=None):
    """`img` is a PIL RGB image; returns the ASTC payload for every mip level.

    Mip dimensions halve independently and stop at 1, which is what Unity's
    chain does, so the payload length matches the size the Texture2D declares.
    """
    height = width if height is None else height
    out = bytearray()
    w, h = width, height
    while True:
        nbx, nby = _blocks(w), _blocks(h)
        # A box resize straight to the block grid averages each 6x6 tile.
        grid = img.resize((nbx, nby))
        px = grid.load()
        for y in range(nby):
            for x in range(nbx):
                out += void_extent(px[x, y][:3])
        if w == 1 and h == 1:
            break
        w = max(1, w // 2)
        h = max(1, h // 2)
    return bytes(out)
