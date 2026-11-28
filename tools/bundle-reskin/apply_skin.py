#!/usr/bin/env python3
"""Apply a named grader module's `grade(image, texture_name)` to a bundle.

Generalises `retheme.py`: the grader decides the look, this handles the parts
that must not vary -- decode the original so material detail survives, re-encode
at the exact size the Texture2D declares, splice in place so no offset moves, and
never touch a normal map, which would break the lighting.

    ./apply_skin.py <src bundle> <dst bundle> <grader module>
"""
import importlib
import os
import sys

import UnityPy
from UnityPy.streams import EndianBinaryReader

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from astc_real import encode, _context  # noqa: E402


def main():
    src, dst, mod_name = sys.argv[1], sys.argv[2], sys.argv[3]
    grader = importlib.import_module(mod_name)
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
            payload = encode(grader.grade(d.image, name), d.m_Width, d.m_Height, ctx)
        except Exception as exc:                            # noqa: BLE001
            skipped.append(f"{name}: {exc}")
            continue
        if len(payload) != sd.size:
            skipped.append(f"{name}: size {len(payload)} != {sd.size}")
            continue
        edits.append((sd.path.split("/")[-1], sd.offset, payload))

    f = env.file
    by_res = {}
    for res, off, payload in edits:
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
    print(f"  {os.path.basename(src)}: {len(edits)} albedos -> {mod_name}"
          + (f", skipped {len(skipped)}" if skipped else "")
          + f", {os.path.getsize(dst):,} bytes")
    for s in skipped[:4]:
        print(f"    skip {s}")


if __name__ == "__main__":
    main()
