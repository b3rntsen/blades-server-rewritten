#!/usr/bin/env python3
"""Paint every albedo in a bundle a unique flat colour and record the mapping.

One launch then attributes every visible surface at once: read a pixel off the
screenshot, look the colour up, and you know which texture draws there. Far
cheaper than noising one bundle at a time and guessing from what breaks.
"""
import json, os, sys
import UnityPy
from UnityPy.streams import EndianBinaryReader

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from astc66 import void_extent, _blocks  # noqa: E402

src, dst, mapfile = sys.argv[1], sys.argv[2], sys.argv[3]


def flat(rgb, w, h):
    out = bytearray()
    while True:
        out += void_extent(rgb) * (_blocks(w) * _blocks(h))
        if w == 1 and h == 1:
            break
        w, h = max(1, w // 2), max(1, h // 2)
    return bytes(out)


def palette(i):
    # Widely separated hues, full saturation, so neighbouring surfaces never
    # read as the same colour after the engine's lighting and fog.
    import colorsys
    r, g, b = colorsys.hsv_to_rgb((i * 0.618033988749895) % 1.0, 1.0, 1.0)
    return (int(r * 255), int(g * 255), int(b * 255))


env = UnityPy.load(src)
mapping, by_res = {}, {}
i = 0
for o in env.objects:
    if o.type.name != "Texture2D":
        continue
    d = o.read()
    if int(d.m_TextureFormat) != 50 or not d.m_Name.lower().endswith("_alb"):
        continue
    sd = d.m_StreamData
    if not (sd and sd.path and sd.size):
        continue
    rgb = palette(i)
    payload = flat(rgb, d.m_Width, d.m_Height)
    if len(payload) != sd.size:
        continue
    i += 1
    mapping["%02x%02x%02x" % rgb] = d.m_Name
    by_res.setdefault(sd.path.split("/")[-1], []).append((sd.offset, payload))

f = env.file
for res_name, items in by_res.items():
    key = next((k for k in f.files if k.endswith(res_name)), None)
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
json.dump(mapping, open(mapfile, "w"), indent=1)
print(f"  {len(mapping)} albedos colour-coded -> {dst}")
