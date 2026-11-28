#!/usr/bin/env python3
"""Assemble the Easter pack from bundles that are already patched.

`build_pack.py` patches a texture itself; these bundles were produced by
`apply_skin.py` and only need packaging. The output directory holds files named
by content hash plus a `pack.json` describing them (see README.md).

Three things go in the pack, and the third is what makes over-the-air delivery
work at all:

  * the patched bundles, by content hash;
  * a manifest variant whose Hash128 for each patched bundle points at that hash;
  * an `_ExtraData` variant carrying each bundle's NEW size and a CRC of 0,
    Unity's documented "do not verify". Without it the client asks for a size and
    CRC it has no record of and the downloader hangs.
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
BIS = os.path.join(HERE, "bisect")
TOOLS = HERE  # the helper scripts are siblings of this file
OUT = os.path.join(HERE, "pack-easter")
VERSION = "1.31.0.4306485.26246488"

# landscape_sab is deliberately absent. Its size does not appear in _ExtraData,
# so `patch_extradata.py` cannot locate its record, and it refuses rather than
# guessing at four bytes that drive every player's download. Records are ordered
# but variable-length and interleaved with asset-path strings, so finding it
# needs a full parser; until then the foliage ships stock and the theme carries
# on the buildings and the ground.
BUNDLES = ["town_textures_sab", "kit_mainmenu_sab"]


def md5(path):
    h = hashlib.md5()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


os.makedirs(OUT, exist_ok=True)
entries = []
size_specs = []

for name in BUNDLES:
    patched = os.path.join(BIS, f"{name}.easter")
    original = os.path.join(BIS, f"{name}.orig")
    new_hash = md5(patched)
    shutil.copy(patched, os.path.join(OUT, new_hash))
    entries.append({
        "bundleName": name,
        "bundleHash": new_hash,
        "sizeBytes": os.path.getsize(patched),
    })
    size_specs.append(f"{os.path.getsize(original)}:{os.path.getsize(patched)}")
    print(f"  {name:22} {os.path.getsize(patched):>11,} bytes -> {new_hash}")

# manifest variant: repoint each patched bundle at its new hash, in sequence
src = os.path.join(HERE, "manifest_stock.bin")
cur = os.path.join(OUT, "_manifest.work")
shutil.copy(src, cur)
for e in entries:
    nxt = cur + ".n"
    subprocess.run([sys.executable, os.path.join(TOOLS, "repoint_manifest.py"),
                    cur, nxt, e["bundleName"], e["bundleHash"]],
                   check=True, capture_output=True)
    os.replace(nxt, cur)
manifest_hash = md5(cur)
os.replace(cur, os.path.join(OUT, manifest_hash))
print(f"  manifest variant       {manifest_hash}")

# _ExtraData variant: new sizes, CRC 0
ed_in = os.path.join(HERE, "android_extradata.bin")
ed_out = os.path.join(OUT, "_extradata.work")
subprocess.run([sys.executable, os.path.join(TOOLS, "patch_extradata.py"),
                ed_in, ed_out] + size_specs, check=True)
ed_hash = md5(ed_out)
os.replace(ed_out, os.path.join(OUT, ed_hash))
print(f"  extradata variant      {ed_hash}")

descriptor = {
    "id": "easter",
    "name": "Easter — spring town and menu",
    "description": ("Spring palette per material with painted eggs on the "
                    "ground. Patches town_textures_sab and kit_mainmenu_sab; "
                    "foliage ships stock (no _ExtraData record for "
                    "landscape_sab)."),
    "manifestHash": manifest_hash,
    "extraDataHash": ed_hash,
    "gameVersion": VERSION,
    "bundles": entries,
}
with open(os.path.join(OUT, "pack.json"), "w") as fh:
    json.dump(descriptor, fh, indent=1)
print(f"\nwrote {OUT}/pack.json")
