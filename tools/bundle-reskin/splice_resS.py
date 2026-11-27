#!/usr/bin/env python3
"""Overwrite named textures' payloads inside a bundle's .resS, in place.

Only the byte ranges the textures declare are touched -- the serialized objects,
the mesh data sharing the same .resS, and every other texture keep their bytes.
That is what makes this safe where a UnityPy image write was not: nothing is
re-encoded and no offset moves.
"""
import os, sys
import UnityPy
from UnityPy.streams import EndianBinaryReader

src, dst, payload_path = sys.argv[1], sys.argv[2], sys.argv[3]
names = set(sys.argv[4:])
payload = open(payload_path, "rb").read()

env = UnityPy.load(src)
targets = {}
for o in env.objects:
    if o.type.name != "Texture2D":
        continue
    d = o.read()
    if d.m_Name in names:
        sd = d.m_StreamData
        targets[d.m_Name] = (sd.offset, sd.size, sd.path.split("/")[-1])
missing = names - targets.keys()
if missing:
    raise SystemExit(f"not found in bundle: {sorted(missing)}")

f = env.file
res_name = next(iter({t[2] for t in targets.values()}))
key = next(k for k in f.files if k.endswith(res_name))
old = f.files[key]
old.Position = 0
buf = bytearray(old.read(old.Length))

for name, (off, size, _) in sorted(targets.items()):
    if len(payload) != size:
        raise SystemExit(f"{name}: payload {len(payload)} != declared {size}")
    buf[off:off + size] = payload
    print(f"  {name}: {size:,} bytes at offset {off:,} replaced")

new = EndianBinaryReader(bytes(buf))
for attr in ("flags", "path", "name", "is_changed"):
    if hasattr(old, attr):
        setattr(new, attr, getattr(old, attr))
if not hasattr(new, "flags"):
    new.flags = 4
f.files[key] = new

out = f.save(packer="original")
open(dst, "wb").write(out)
print(f"  wrote {dst}: {len(out):,} bytes (was {os.path.getsize(src):,})")
