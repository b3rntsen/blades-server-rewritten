#!/usr/bin/env python3
"""Find each bundle's record in _ExtraData and read the word beside its size.

Sizes are stored as u32le, so a bundle whose size we know locates its record.
Whatever sits next to it is the CRC the client hands to Unity's cached download
-- and Unity REJECTS a mismatching CRC, so this word is the single thing standing
between us and shipping our own art under a stock hash.
"""
import glob
import os
import struct
import zlib

# Resolved relative to this file, and overridable, so the script runs on any
# clone. Both used to name one session's scratchpad, which no longer exists.
HERE = os.path.dirname(os.path.abspath(__file__))
ED = os.environ.get("BLADES_EXTRADATA", os.path.join(HERE, "android_extradata.bin"))
BIS = os.environ.get("BLADES_BISECT", os.path.join(HERE, "bisect"))

blob = open(ED, "rb").read()
print(f"ExtraData {len(blob):,} bytes, header count="
      f"{struct.unpack_from('<H', blob, 4)[0]}\n")

for path in sorted(glob.glob(os.path.join(BIS, "*.orig"))):
    name = os.path.basename(path)[:-5]
    data = open(path, "rb").read()
    size = len(data)
    pat = struct.pack("<I", size)
    offs = [i for i in range(len(blob) - 3) if blob[i:i + 4] == pat]
    if not offs:
        print(f"{name:28} size {size:>11,}  NO RECORD")
        continue
    off = offs[0]
    ctx = blob[max(0, off - 8):off + 16]
    words = struct.unpack_from("<4I", blob, off) if off + 16 <= len(blob) else ()
    print(f"{name:28} size {size:>11,} @0x{off:06x} x{len(offs)}")
    print(f"    ctx  {ctx.hex()}")
    print(f"    next words: " + " ".join(f"0x{w:08x}" for w in words[1:]))

    # Is the neighbouring word any familiar checksum of the file?
    if len(words) > 1:
        cand = words[1]
        for label, val in (
            ("crc32(file)", zlib.crc32(data) & 0xFFFFFFFF),
            ("adler32(file)", zlib.adler32(data) & 0xFFFFFFFF),
        ):
            if val == cand:
                print(f"    -> neighbour MATCHES {label}")
