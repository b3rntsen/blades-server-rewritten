#!/usr/bin/env python3
"""Pin down what the _ExtraData CRC word actually is.

Unity writes a `CRC:` line into each bundle's build .manifest, and the APK ships
both the bundle and its .manifest for the 117 local bundles. If that CRC equals
the word sitting next to the size in _ExtraData, the definition is settled: it
is Unity's build-pipeline CRC, which is what the client hands to the cached
download and what any replacement bundle must reproduce.
"""
import os
import re
import struct
import zipfile
import zlib

# Resolved relative to this file, and overridable, so the script runs on any
# clone. It used to name one laptop's checkout and one session's scratchpad.
HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
APK = os.environ.get("BLADES_APK", os.path.join(ROOT, "reference", "apk", "blades.apk"))
ED = os.environ.get("BLADES_EXTRADATA", os.path.join(HERE, "android_extradata.bin"))

blob = open(ED, "rb").read()
z = zipfile.ZipFile(APK)

names = [n for n in z.namelist()
         if n.startswith("assets/Bundles/") and n.endswith(".manifest")]
print(f"{len(names)} build manifests in the APK\n")
print(f"{'bundle':30} {'apk size':>10} {'manifest CRC':>12} {'ED size@':>10} "
      f"{'ED crc':>12} match")

checked = hits = 0
for m in sorted(names)[:40]:
    base = m[len("assets/Bundles/"):-len(".manifest")]
    try:
        data = z.read(f"assets/Bundles/{base}")
    except KeyError:
        continue
    text = z.read(m).decode("utf-8", "replace")
    mo = re.search(r"CRC:\s*(\d+)", text)
    if not mo:
        continue
    mcrc = int(mo.group(1)) & 0xFFFFFFFF
    size = len(data)
    off = blob.find(struct.pack("<I", size))
    if off < 0:
        continue
    ed_crc = struct.unpack_from("<I", blob, off + 4)[0]
    checked += 1
    ok = ed_crc == mcrc
    hits += ok
    print(f"{base[:30]:30} {size:>10,} {mcrc:>12} {off:>10} "
          f"{ed_crc:>12} {'YES' if ok else 'no'}")
    if ok and hits == 1:
        print(f"    zlib.crc32(file) = {zlib.crc32(data) & 0xFFFFFFFF} "
              f"(for comparison)")

print(f"\n{hits}/{checked} manifests whose CRC equals the word after the size")
