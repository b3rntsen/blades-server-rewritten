"""Encode a PIL image to ASTC_RGB_6x6 with a full mip chain, at real quality.

The void-extent writer in `astc66.py` needs no dependency but quantises every
6x6 tile to one flat colour, which is fine for probes and unacceptable for art
that sits under the camera. This uses the actual astcenc encoder, so a re-skinned
surface looks native. The payload length still has to match the size the
Texture2D declares exactly, and it does: the block count per mip is the same
arithmetic either way.
"""
import astc_encoder as ae
from PIL import Image

BLOCK = 6


def _blocks(d):
    return (d + BLOCK - 1) // BLOCK


def _context():
    cfg = ae.ASTCConfig(ae.ASTCProfile.LDR_SRGB, BLOCK, BLOCK, 1,
                        ae.ASTCQualityPreset.MEDIUM)
    return ae.ASTCContext(cfg)


def encode(img, width=512, height=None, ctx=None):
    height = width if height is None else height
    ctx = ctx or _context()
    out = bytearray()
    w, h = width, height
    while True:
        level = img.resize((w, h), Image.LANCZOS).convert("RGBA")
        image = ae.ASTCImage(ae.ASTCType.U8, w, h, 1, data=level.tobytes())
        swz = ae.ASTCSwizzle.from_str("RGBA")
        out += ctx.compress(image, swz)
        if w == 1 and h == 1:
            break
        w, h = max(1, w // 2), max(1, h // 2)
    return bytes(out)


def payload_size(width, height):
    w, h, n = width, height, 0
    while True:
        n += _blocks(w) * _blocks(h)
        if w == 1 and h == 1:
            break
        w, h = max(1, w // 2), max(1, h // 2)
    return n * 16
