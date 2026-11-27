#!/usr/bin/env python3
"""Splice one art module's draw() onto named textures in a bundle."""
import importlib, os, sys
import UnityPy
from UnityPy.streams import EndianBinaryReader
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from astc66 import encode

src, dst, module = sys.argv[1], sys.argv[2], sys.argv[3]
names = set(sys.argv[4:])
art = importlib.import_module(module).draw()

env = UnityPy.load(src)
edits = []
for o in env.objects:
    if o.type.name != "Texture2D":
        continue
    d = o.read()
    if d.m_Name not in names or int(d.m_TextureFormat) != 50:
        continue
    sd = d.m_StreamData
    payload = encode(art, d.m_Width, d.m_Height)
    assert len(payload) == sd.size, f"{d.m_Name}: {len(payload)} != {sd.size}"
    edits.append((d.m_Name, sd.path.split("/")[-1], sd.offset, payload))
missing = names - {e[0] for e in edits}
if missing:
    raise SystemExit(f"not found: {sorted(missing)}")

f = env.file
for name, res, off, payload in edits:
    key = next(k for k in f.files if k.endswith(res))
    old = f.files[key]
    old.Position = 0
    buf = bytearray(old.read(old.Length))
    buf[off:off + len(payload)] = payload
    new = EndianBinaryReader(bytes(buf))
    for attr in ("flags", "path", "name", "is_changed"):
        if hasattr(old, attr):
            setattr(new, attr, getattr(old, attr))
    if not hasattr(new, "flags"):
        new.flags = 4
    f.files[key] = new
    print(f"  {name}: {len(payload):,} bytes at {off:,}")
open(dst, "wb").write(f.save(packer="original"))
print(f"  wrote {dst}")
