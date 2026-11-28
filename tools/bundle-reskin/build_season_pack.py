#!/usr/bin/env python3
"""Build one season's pack: patched bundles, manifest variant, _ExtraData variant.

    ./build_season_pack.py <grader module> <pack id> "<display name>"

Generalises `build_easter_pack.py`. Only the two bundles whose size can be found
in `_ExtraData` are patched -- `patch_extradata.py` refuses to guess at a record,
and guessing would corrupt an unrelated bundle for every player.
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
VERSION = "1.31.0.4306485.26246488"

# landscape_sab is absent on purpose: its size does not appear in _ExtraData, so
# its record cannot be located. Foliage ships stock in every season until the
# record format is fully parsed.
BUNDLES = ["town_textures_sab", "kit_mainmenu_sab"]


def md5(path):
    h = hashlib.md5()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main():
    grader, pack_id, display = sys.argv[1], sys.argv[2], sys.argv[3]
    out = os.path.join(HERE, "pack-" + pack_id)
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)

    entries, size_specs = [], []
    for name in BUNDLES:
        src = os.path.join(BIS, name + ".orig")
        patched = os.path.join(BIS, "%s.%s" % (name, pack_id))
        subprocess.run([sys.executable, os.path.join(TOOLS, "apply_skin.py"),
                        src, patched, grader], check=True)
        h = md5(patched)
        shutil.copy(patched, os.path.join(out, h))
        entries.append({"bundleName": name, "bundleHash": h,
                        "sizeBytes": os.path.getsize(patched)})
        size_specs.append("%d:%d" % (os.path.getsize(src),
                                     os.path.getsize(patched)))

    cur = os.path.join(out, "_manifest.work")
    shutil.copy(os.path.join(HERE, "manifest_stock.bin"), cur)
    for e in entries:
        nxt = cur + ".n"
        subprocess.run([sys.executable, os.path.join(TOOLS, "repoint_manifest.py"),
                        cur, nxt, e["bundleName"], e["bundleHash"]],
                       check=True, capture_output=True)
        os.replace(nxt, cur)
    manifest_hash = md5(cur)
    os.replace(cur, os.path.join(out, manifest_hash))

    ed_out = os.path.join(out, "_extradata.work")
    subprocess.run([sys.executable, os.path.join(TOOLS, "patch_extradata.py"),
                    os.path.join(HERE, "android_extradata.bin"), ed_out]
                   + size_specs, check=True)
    ed_hash = md5(ed_out)
    os.replace(ed_out, os.path.join(out, ed_hash))

    json.dump({
        "id": pack_id,
        "name": display,
        "description": ("Seasonal re-skin of the town and main menu. Patches "
                        "town_textures_sab and kit_mainmenu_sab; foliage ships "
                        "stock (no _ExtraData record for landscape_sab)."),
        "manifestHash": manifest_hash,
        "extraDataHash": ed_hash,
        "gameVersion": VERSION,
        "bundles": entries,
    }, open(os.path.join(out, "pack.json"), "w"), indent=1)
    print("  manifest %s   extradata %s" % (manifest_hash, ed_hash))
    print("  wrote %s/pack.json" % out)


if __name__ == "__main__":
    main()
