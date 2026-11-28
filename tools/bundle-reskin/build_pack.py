#!/usr/bin/env python3
"""Build an asset pack: patched bundles, a manifest variant, and a descriptor.

One command turns "this PNG, on that texture" into everything the box needs:

    ./build_pack.py \\
        --id halloween --name "Halloween pumpkin" \\
        --manifest /path/to/Manifests/1.31.0.4306485.26246488 \\
        --bundle ui_textures_ui_logo_sab=/path/to/f0fed3de... \\
        --texture UI_Logo_Blades_White --overlay pumpkin.png \\
        --out ./pack-halloween

The output directory holds files named by content hash plus a `pack.json`.
To serve it, copy the hash-named bundles into your bundle mirror under
`Android/Bundles/<first two hex>/<hash>` and the manifest variant over
`Android/Manifests/<version>` (see README.md, "Delivering a pack").

## Why this runs here and not on the server

Rewriting a bundle needs UnityPy. Rather than install a build toolchain onto
the host that serves every player's download, the artifacts are produced on a
workstation and copied over. The server only ever copies bytes and edits one JSON file.

## What a "manifest variant" is

A copy of the real manifest with the patched bundles' `Hash128` values swapped
in. The client is told this variant's identifier through `MetaManifest`, fetches
it, and then asks for the new bundle hashes — which is the whole update
mechanism. See README.md.
"""

import argparse
import hashlib
import json
import os
import shutil
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from patch_bundle_texture import patch  # noqa: E402
from repoint_manifest import repoint  # noqa: E402


def _digest(path):
    with open(path, "rb") as fh:
        return hashlib.md5(fh.read()).hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--id", required=True, help="stable pack id, e.g. halloween")
    ap.add_argument("--name", required=True)
    ap.add_argument("--description", default=None)
    ap.add_argument("--manifest", required=True, help="the stock manifest file")
    ap.add_argument(
        "--bundle",
        action="append",
        required=True,
        metavar="NAME=PATH",
        help="logical bundle name and the stock file to patch; repeatable",
    )
    ap.add_argument("--texture", required=True)
    grp = ap.add_mutually_exclusive_group(required=True)
    grp.add_argument("--overlay")
    grp.add_argument("--replace")
    ap.add_argument("--pos", default="bottom-right")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    bundles = []

    for spec in args.bundle:
        if "=" not in spec:
            raise SystemExit(f"--bundle wants NAME=PATH, got {spec!r}")
        name, src = spec.split("=", 1)
        staged = os.path.join(args.out, f"{name}.patched")
        digest, size = patch(
            src, staged, args.texture, args.overlay, args.replace, args.pos
        )
        final = os.path.join(args.out, digest)
        os.replace(staged, final)
        bundles.append({"bundleName": name, "bundleHash": digest, "sizeBytes": size})
        print(f"  {name}: {size:,} bytes -> {digest}")

    # One manifest carrying every bundle this pack changes.
    variant = os.path.join(args.out, "manifest.variant")
    shutil.copy(args.manifest, variant)
    for b in bundles:
        staged = variant + ".next"
        repoint(variant, staged, b["bundleName"], b["bundleHash"])
        os.replace(staged, variant)

    manifest_hash = _digest(variant)
    os.replace(variant, os.path.join(args.out, manifest_hash))
    print(f"  manifest variant -> {manifest_hash}")

    descriptor = {
        "id": args.id,
        "name": args.name,
        "description": args.description,
        "manifestHash": manifest_hash,
        "bundles": bundles,
    }
    with open(os.path.join(args.out, "pack.json"), "w") as fh:
        json.dump(descriptor, fh, indent=1)
        fh.write("\n")

    print(f"\nwrote {args.out}/pack.json")
    print("files to copy into the mirror:")
    for b in bundles:
        print(f"  {b['bundleHash']}")
    print(f"  {manifest_hash}")


if __name__ == "__main__":
    main()
