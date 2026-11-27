#!/usr/bin/env python3
"""Point an `_ExtraData` entry at the NEW bundle hash, not just fix its size.

`patch_extradata.py` rewrote each record's size and CRC, which is necessary and
not sufficient: the client looks a bundle up in `_ExtraData` **by Hash128**, and
the manifest variant points it at a hash that has no entry at all. It then hangs
on "checking for updates" having asked for nothing -- the exact symptom, observed
with the variant confirmed present on the device.

The table is 16-byte raw Hash128 entries from offset 6, so the old hash is
replaced in place with the new one. Length is unchanged, so every following
offset is untouched.

Caveat worth stating: the table is alphabetical, and swapping a hash breaks that
ordering. If the client binary-searches it, this will not work and the lookup
needs the table re-sorted instead. That is the next thing to try if this still
hangs.

    ./repoint_extradata_hash.py <in> <out> <oldhash>:<newhash> [...]
"""
import sys


def main():
    src, dst = sys.argv[1], sys.argv[2]
    blob = bytearray(open(src, "rb").read())
    for spec in sys.argv[3:]:
        old_hex, new_hex = spec.split(":")
        old, new = bytes.fromhex(old_hex), bytes.fromhex(new_hex)
        if len(old) != 16 or len(new) != 16:
            raise SystemExit("hashes must be 16 bytes (32 hex chars)")
        hits = [i for i in range(len(blob) - 15) if blob[i:i + 16] == old]
        if not hits:
            raise SystemExit("old hash %s not present" % old_hex)
        if len(hits) > 1:
            raise SystemExit(
                "old hash %s occurs %d times; refusing to guess" % (old_hex, len(hits)))
        blob[hits[0]:hits[0] + 16] = new
        print("  %s -> %s at offset %d (table index %d)"
              % (old_hex[:12], new_hex[:12], hits[0], (hits[0] - 6) // 16))
    open(dst, "wb").write(bytes(blob))
    print("  wrote %s (%d bytes)" % (dst, len(blob)))


if __name__ == "__main__":
    main()
