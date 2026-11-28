#!/usr/bin/env python3
"""Noise only the byte ranges Texture2D objects declare in the .resS.

Noising a whole .resS also destroys the mesh vertex buffers that share it, which
makes the result unattributable -- geometry explodes whether or not the bundle's
textures are on screen. Restricting the damage to declared texture ranges leaves
the meshes exact, so anything that turns to static is a texture actually drawn.
"""
import os, sys, random
import UnityPy
from UnityPy.streams import EndianBinaryReader

src, dst = sys.argv[1], sys.argv[2]
random.seed(99)
env = UnityPy.load(src)
ranges = {}
for o in env.objects:
    if o.type.name != "Texture2D":
        continue
    d = o.read()
    sd = getattr(d, "m_StreamData", None)
    if sd and sd.path and sd.size:
        ranges.setdefault(sd.path.split("/")[-1], []).append(
            (d.m_Name, sd.offset, sd.size))
if not ranges:
    print(f"  {os.path.basename(src)}: no streamed textures -- skipped")
    sys.exit(2)

f = env.file
total = 0
for res_name, items in ranges.items():
    key = next((k for k in f.files if k.endswith(res_name)), None)
    if key is None:
        continue
    old = f.files[key]
    old.Position = 0
    buf = bytearray(old.read(old.Length))
    for name, off, size in items:
        buf[off:off + size] = random.randbytes(size)
        total += 1
    new = EndianBinaryReader(bytes(buf))
    for attr in ("flags", "path", "name", "is_changed"):
        if hasattr(old, attr):
            setattr(new, attr, getattr(old, attr))
    if not hasattr(new, "flags"):
        new.flags = 4
    f.files[key] = new

out = f.save(packer="original")
open(dst, "wb").write(out)
print(f"  {os.path.basename(src)}: {total} textures noised, "
      f"{len(out):,} bytes (was {os.path.getsize(src):,})")
