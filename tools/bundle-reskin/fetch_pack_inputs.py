#!/usr/bin/env python3
"""Fetch the stock inputs a season pack is built from.

Every grader and builder in this directory is committed, but until now their
three inputs were not, and no step anywhere said where to get them:

  * ``manifest_stock.bin``   — the stock Android AssetBundleManifest
  * ``android_extradata.bin``— the stock ``<version>_ExtraData``
  * ``bundle_hashes.json``   — bundle name -> Hash128, derived from the manifest
  * ``bisect/<name>.orig``   — the stock bytes of each bundle being patched

None of them is committed (they are game data), and `build_pack_v2.py` opens
all of them by name, so run this once before building a pack.

All of it is derivable from a bundle mirror: the
``bundles.blades.bgs.services/Android`` directory the server serves from its
static-data dir (``<--static-data>/bundles.blades.bgs.services/Android``), built
from your own game install / capture. That mirror is the source of truth for
what clients actually download (the APK's copies are a stale build-time
snapshot and hash differently, so they are NOT interchangeable).

    ./fetch_pack_inputs.py --from-dir MIRROR/Android                     # manifest + extradata + hashes
    ./fetch_pack_inputs.py --from-dir MIRROR/Android audio_sab town_textures_sab   # ...and these originals

Note the mirror stores each bundle under ``Bundles/<first two hex>/<hash>`` and
the hash is Unity's Hash128 — a CACHE KEY, not a checksum of the file. Do not
"verify" a download by hashing it; it will not match, and that is correct.
"""
import argparse
import json
import os
import shutil
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
BIS = os.path.join(HERE, "bisect")

VERSION = "1.31.0.4306485.26246488"


def fetch(rel, dest, args):
    """Copy one mirror-relative path to dest."""
    src = os.path.join(args.from_dir, rel)
    if not os.path.exists(src):
        raise SystemExit("not in --from-dir: %s" % src)
    shutil.copyfile(src, dest)
    return os.path.getsize(dest)


def bundle_hashes(manifest_path):
    """name -> 32-hex Hash128, read out of the manifest bundle itself.

    Deriving this rather than shipping it keeps one source of truth: the
    manifest the client is actually served.
    """
    import UnityPy  # imported late so --help works without it installed

    env = UnityPy.load(manifest_path)
    for obj in env.objects:
        if obj.type.name != "AssetBundleManifest":
            continue
        tree = obj.read_typetree()
        names = {}
        for entry in tree["AssetBundleNames"]:
            idx, name = (entry["first"], entry["second"]) if isinstance(entry, dict) else entry
            names[idx] = name
        out = {}
        for entry in tree["AssetBundleInfos"]:
            idx, info = (entry["first"], entry["second"]) if isinstance(entry, dict) else entry
            h = info["AssetBundleHash"]
            out[names[idx]] = "".join("%02x" % h["bytes[%d]" % i] for i in range(16))
        return out
    raise SystemExit("no AssetBundleManifest in %s" % manifest_path)


def mirror_sizes(hashes, args):
    """bundle name -> served byte size, for every bundle the mirror holds."""
    out = {}
    for name, h in hashes.items():
        path = os.path.join(args.from_dir, "Bundles", h[:2], h)
        if os.path.exists(path):
            out[name] = os.path.getsize(path)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("bundles", nargs="*",
                    help="bundle names whose stock bytes to fetch into bisect/")
    ap.add_argument("--from-dir", required=True,
                    help="a bundle mirror's Android/ dir (holds Manifests/ and Bundles/)")
    ap.add_argument("--version", default=VERSION)
    args = ap.parse_args()

    manifest = os.path.join(HERE, "manifest_stock.bin")
    extradata = os.path.join(HERE, "android_extradata.bin")

    print("stock manifest  %d bytes"
          % fetch("Manifests/%s" % args.version, manifest, args))
    print("stock ExtraData %d bytes"
          % fetch("Manifests/%s_ExtraData" % args.version, extradata, args))

    hashes = bundle_hashes(manifest)
    with open(os.path.join(HERE, "bundle_hashes.json"), "w") as fh:
        json.dump(hashes, fh, indent=1, sort_keys=True)
    print("bundle_hashes.json  %d bundles" % len(hashes))

    # Served sizes, which exist nowhere but the mirror. `extradata.locate` needs
    # them to triangulate a bundle's size/CRC record: a bundle whose size occurs
    # exactly once in _ExtraData pins the offset of the run it sits in, and its
    # neighbours are then reachable by index. Bundles the mirror has never been
    # asked for are simply absent, and are skipped rather than recorded as 0.
    sizes = mirror_sizes(hashes, args)
    with open(os.path.join(HERE, "bundle_sizes.json"), "w") as fh:
        json.dump(sizes, fh, indent=1, sort_keys=True)
    print("bundle_sizes.json   %d of %d bundles present on the mirror"
          % (len(sizes), len(hashes)))

    if not args.bundles:
        return
    os.makedirs(BIS, exist_ok=True)
    for name in args.bundles:
        if name not in hashes:
            raise SystemExit("no such bundle in the manifest: %s" % name)
        h = hashes[name]
        dest = os.path.join(BIS, name + ".orig")
        size = fetch("Bundles/%s/%s" % (h[:2], h), dest, args)
        print("  %-34s %s  %d bytes" % (name, h[:12], size))
        # A bundle the mirror has never been asked for is simply absent, and a
        # zero-length file would be patched into a pack that bricks the client.
        if size == 0:
            raise SystemExit("%s came back empty — is it in the mirror?" % name)


if __name__ == "__main__":
    main()
