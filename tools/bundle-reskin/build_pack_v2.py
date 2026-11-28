#!/usr/bin/env python3
"""Build a pack from already-patched bundles, doing BOTH _ExtraData edits.

The first builder only rewrote each record's size and CRC, which is half the
job: the client looks a bundle up in `_ExtraData` **by Hash128**, and the
manifest variant points it at a hash with no entry, so it asks for nothing and
hangs. Both edits are needed, in this order, per bundle:

  1. size -> the new size, crc -> 0 (Unity's documented "do not verify")
  2. the old Hash128 in the table -> the new one

Edit 2 is located by the old Hash128, which must occur exactly once. Edit 1 used
to be located the same way, by searching for the bundle's current size — which
is not a locator but a coincidence detector, and which cannot find the record at
all once the served bytes have drifted from what the record says (`common` on
the mirror is 429 bytes larger than its record). It now goes through
`extradata.locate`, which finds the record by the bundle's INDEX in the
alphabetical table and proves the answer with neighbouring bundles whose served
size is unambiguous. That needs `bundle_sizes.json` — `fetch_pack_inputs.py`
writes it.

    ./build_pack_v2.py <pack id> "<name>" <bundle>=<patched file> [...]
"""
import hashlib
import json
import os
import shutil
import struct
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import extradata  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
BIS = os.path.join(HERE, "bisect")
TOOLS = HERE  # the helper scripts are siblings of this file
VERSION = "1.31.0.4306485.26246488"
#: Packs are not all re-skins — a data pack (e.g. a raised inventory cap)
#: needs its own wording; the description is recorded in pack.json for
#: whoever decides whether to ship it.
DESCRIPTION = os.environ.get(
    "PACK_DESCRIPTION", "Seasonal re-skin: graded albedos plus night lighting.")


def md5(path):
    h = hashlib.md5()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def unique(blob, needle, what):
    hits = [i for i in range(len(blob) - len(needle) + 1)
            if blob[i:i + len(needle)] == needle]
    if len(hits) != 1:
        raise SystemExit("%s occurs %d times — refusing to guess" % (what, len(hits)))
    return hits[0]


def main():
    pack_id, display = sys.argv[1], sys.argv[2]
    specs = [a.split("=", 1) for a in sys.argv[3:]]
    hashes = json.load(open(os.path.join(HERE, "bundle_hashes.json")))
    out = os.path.join(HERE, "pack-" + pack_id)
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)

    sizes = json.load(open(os.path.join(HERE, "bundle_sizes.json")))
    names = sorted(hashes)
    ed = bytearray(open(os.path.join(HERE, "android_extradata.bin"), "rb").read())
    entries = []

    for name, patched in specs:
        # `name=file@needle`: the scene bundles have too few anchors for
        # `locate`, so they are found by a record that names their own assets.
        patched, _, needle = patched.partition("@")
        # `name=file#0xOFFSET`: a record proven elsewhere (e.g. one a shipped pack
        # already edited). Accepted only if the stock record there holds exactly
        # the bundle's served size.
        patched, _, pinned = patched.partition("#")
        new_size = os.path.getsize(patched)
        old_hash, new_hash = hashes[name], md5(patched)

        if pinned:
            off = int(pinned, 16)
            if extradata.read(bytes(ed), off)[0] != sizes[name]:
                raise SystemExit("%s: record at %s does not hold the served size %d"
                                 % (name, pinned, sizes[name]))
            anchors = ["pinned:" + pinned]
        elif needle:
            off = extradata.locate_by_identity(bytes(ed), name, sizes[name], needle)
            anchors = ["identity:" + needle]
        else:
            off, anchors = extradata.locate(bytes(ed), names, name, sizes)
        was = extradata.read(bytes(ed), off)
        struct.pack_into("<I", ed, off, new_size)
        struct.pack_into("<I", ed, off + 4, 0)          # crc 0 = do not verify
        print("  %-34s record @0x%x  size %d -> %d, crc -> 0  (anchors: %s)"
              % (name, off, was[0], new_size, ", ".join(anchors[:2])))

        hoff = unique(ed, bytes.fromhex(old_hash), "%s hash entry" % name)
        ed[hoff:hoff + 16] = bytes.fromhex(new_hash)

        shutil.copy(patched, os.path.join(out, new_hash))
        entries.append({"bundleName": name, "bundleHash": new_hash,
                        "sizeBytes": new_size})
        print("  %-34s %s -> %s  (%d bytes)"
              % (name, old_hash[:12], new_hash[:12], new_size))

    cur = os.path.join(out, "_m.work")
    shutil.copy(os.path.join(HERE, "manifest_stock.bin"), cur)
    for e in entries:
        nxt = cur + ".n"
        subprocess.run([sys.executable, os.path.join(TOOLS, "repoint_manifest.py"),
                        cur, nxt, e["bundleName"], e["bundleHash"]],
                       check=True, capture_output=True)
        os.replace(nxt, cur)
    manifest_hash = md5(cur)
    os.replace(cur, os.path.join(out, manifest_hash))

    ed_path = os.path.join(out, "_ed.work")
    open(ed_path, "wb").write(bytes(ed))
    ed_hash = md5(ed_path)
    os.replace(ed_path, os.path.join(out, ed_hash))

    json.dump({"id": pack_id, "name": display,
               "description": DESCRIPTION,
               "manifestHash": manifest_hash, "extraDataHash": ed_hash,
               "gameVersion": VERSION, "bundles": entries},
              open(os.path.join(out, "pack.json"), "w"), indent=1)
    print("  manifest %s   extradata %s" % (manifest_hash, ed_hash))
    print("  wrote %s/pack.json" % out)


if __name__ == "__main__":
    main()
