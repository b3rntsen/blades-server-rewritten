#!/usr/bin/env python3
"""Re-theme a bundle's albedos by grading the ORIGINAL art, not replacing it.

The UV probe proved why this is the only honest way to skin this scene: the
architecture is tiled and atlas-packed, so a picture pasted into a texture gets
shredded across unrelated faces. What survives that layout is a change of
palette -- which is what a seasonal re-skin actually is.

Each texture is decoded, pushed through a duotone ramp keyed off its own
luminance, blended back over the original so material detail survives, and
re-encoded with the real ASTC encoder at the exact declared size. Normal maps
are never touched: grading them would break the lighting.
"""
import os, sys
import UnityPy
from PIL import Image, ImageOps
from UnityPy.streams import EndianBinaryReader

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from astc_real import encode, _context  # noqa: E402

# deep bruise-purple shadows -> ember orange highlights
SHADOW = (26, 14, 38)
MID = (122, 52, 24)
HIGHLIGHT = (255, 176, 66)
STRENGTH = 0.72


def _ramp():
    lut = []
    for i in range(256):
        t = i / 255.0
        if t < 0.5:
            u = t / 0.5
            c = [SHADOW[k] + (MID[k] - SHADOW[k]) * u for k in range(3)]
        else:
            u = (t - 0.5) / 0.5
            c = [MID[k] + (HIGHLIGHT[k] - MID[k]) * u for k in range(3)]
        lut.append(tuple(int(v) for v in c))
    return lut


_LUT = _ramp()


def grade(img):
    rgb = img.convert("RGB")
    lum = ImageOps.grayscale(rgb)
    duo = Image.merge("RGB", [
        lum.point([_LUT[i][ch] for i in range(256)]) for ch in range(3)])
    return Image.blend(rgb, duo, STRENGTH)


def main():
    src, dst = sys.argv[1], sys.argv[2]
    env = UnityPy.load(src)
    ctx = _context()
    edits, skipped = [], []
    for o in env.objects:
        if o.type.name != "Texture2D":
            continue
        d = o.read()
        name = d.m_Name
        if int(d.m_TextureFormat) != 50 or not name.lower().endswith("_alb"):
            continue
        sd = d.m_StreamData
        if not (sd and sd.path and sd.size):
            continue
        try:
            payload = encode(grade(d.image), d.m_Width, d.m_Height, ctx)
        except Exception as exc:                      # noqa: BLE001
            skipped.append(f"{name} ({exc})")
            continue
        if len(payload) != sd.size:
            skipped.append(f"{name} (size {len(payload)} != {sd.size})")
            continue
        edits.append((name, sd.path.split("/")[-1], sd.offset, payload))

    f = env.file
    by_res = {}
    for name, res, off, payload in edits:
        by_res.setdefault(res, []).append((off, payload))
    for res, items in by_res.items():
        key = next((k for k in f.files if k.endswith(res)), None)
        if key is None:
            continue
        old = f.files[key]
        old.Position = 0
        buf = bytearray(old.read(old.Length))
        for off, payload in items:
            buf[off:off + len(payload)] = payload
        new = EndianBinaryReader(bytes(buf))
        for attr in ("flags", "path", "name", "is_changed"):
            if hasattr(old, attr):
                setattr(new, attr, getattr(old, attr))
        if not hasattr(new, "flags"):
            new.flags = 4
        f.files[key] = new

    open(dst, "wb").write(f.save(packer="original"))
    print(f"  {os.path.basename(src)}: graded {len(edits)} albedos"
          + (f", skipped {len(skipped)}" if skipped else ""))
    for s in skipped[:5]:
        print(f"    skip {s}")


if __name__ == "__main__":
    main()
