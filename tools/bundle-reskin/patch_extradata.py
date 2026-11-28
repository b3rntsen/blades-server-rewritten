#!/usr/bin/env python3
"""Rewrite a bundle's size and CRC inside `<version>_ExtraData`.

The client asks this file for a bundle's size and CRC before downloading, and
hands the CRC to Unity's cached download, which refuses content that does not
match. A bundle we authored can never reproduce Bethesda's CRC, so we write our
own record: the real new size, and **0 for the CRC**, which is Unity's documented
"do not verify".

Records are located by the ORIGINAL size, and only when that value occurs exactly
once in the file. That is the safety property here -- this file drives every
player's download, and silently patching the wrong four bytes would corrupt an
unrelated bundle. An ambiguous or missing size is refused, never guessed.

    ./patch_extradata.py <in> <out> <origsize>:<newsize> [...]
"""
import struct
import sys


def patch(blob, original_size, new_size, new_crc=0):
    """Return (patched blob, offset). Raises if the record is not unique."""
    pat = struct.pack("<I", original_size)
    offs = [i for i in range(len(blob) - 7) if blob[i:i + 4] == pat]
    if not offs:
        raise SystemExit(f"no record found for size {original_size}")
    if len(offs) > 1:
        raise SystemExit(
            f"size {original_size} occurs {len(offs)} times; refusing to guess")
    off = offs[0]
    out = bytearray(blob)
    struct.pack_into("<I", out, off, new_size)
    struct.pack_into("<I", out, off + 4, new_crc)
    return bytes(out), off


def main():
    src, dst = sys.argv[1], sys.argv[2]
    blob = open(src, "rb").read()
    for spec in sys.argv[3:]:
        old_s, new_s = (int(x) for x in spec.split(":"))
        blob, off = patch(blob, old_s, new_s)
        print(f"  record @0x{off:06x}: size {old_s:,} -> {new_s:,}, crc -> 0")
    open(dst, "wb").write(blob)
    print(f"  wrote {dst} ({len(blob):,} bytes)")


if __name__ == "__main__":
    main()
