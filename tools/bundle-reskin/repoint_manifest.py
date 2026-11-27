#!/usr/bin/env python3
"""Point one bundle in the manifest at a new Hash128.

Unity's ``Hash128`` here is a **cache key**: a client re-downloads a bundle when
the manifest's hash for it differs from what its cache holds. So shipping an
update is exactly this edit plus serving the new file — no Bethesda tooling and
no Unity editor licence.

    ./repoint_manifest.py MANIFEST_IN MANIFEST_OUT ui_textures_ui_logo_sab <new-hash>

The manifest is itself a plain UnityFS bundle holding an ``AssetBundleManifest``
with ``AssetBundleNames`` + ``AssetBundleInfos``.

Afterwards, bump ``manifestHash`` for this version in ``MetaManifest.json`` — or
the client keeps its cached manifest and never learns the bundle moved. That
field is a cache key too, not a checksum of the file: the shipped value is not
the manifest's md5, which was checked.

The ``_ExtraData`` sibling is no longer unresolved. The client asks it for a
bundle's size and CRC through ``IAssetBundleExtraDataProvider`` and hands the CRC
to Unity's cached download, which refuses content that does not match -- so a
repointed hash with no record there hangs the downloader. Write a record for the
new hash with ``patch_extradata.py``: the real size, and 0 for the CRC, which is
Unity's documented "do not verify". See ``docs/custom-assets-delivery.md``.
"""

import argparse
import hashlib
import os

import UnityPy


def _assert_written(path, bundle, expected):
    """Read the output back. This tool changes 16 bytes; proving they landed is
    two lines, and a silently dropped save_typetree looks identical without it."""
    env = UnityPy.load(path)
    for obj in env.objects:
        tree = obj.read_typetree()
        if "AssetBundleInfos" not in tree:
            continue
        names = {i: n for i, n in tree["AssetBundleNames"]}
        for idx, info in tree["AssetBundleInfos"]:
            if names.get(idx) != bundle:
                continue
            got = "".join(
                f"{info['AssetBundleHash'][f'bytes[{k}]']:02x}" for k in range(16)
            )
            if got != expected:
                raise SystemExit(f"write-back check failed: {path} has {got}")
            return
    raise SystemExit(f"write-back check failed: {bundle} not found in {path}")


def repoint(src, dst, bundle, new_hash):
    """Rewrite ``bundle``'s AssetBundleHash. Returns ``(old_hash, new_file_md5)``."""
    try:
        raw = bytes.fromhex(new_hash)
    except ValueError:
        raise SystemExit(f"not hexadecimal: {new_hash!r}") from None
    if len(raw) != 16:
        raise SystemExit(
            f"hash must be 32 hex characters (a Hash128), got {len(new_hash)}"
        )

    if os.path.abspath(src) == os.path.abspath(dst):
        raise SystemExit("src and dst are the same file; refusing to edit in place")

    env = UnityPy.load(src)
    for obj in env.objects:
        tree = obj.read_typetree()
        if "AssetBundleInfos" not in tree:
            continue

        names = {i: n for i, n in tree["AssetBundleNames"]}
        target = {n: i for i, n in names.items()}.get(bundle)
        if target is None:
            raise SystemExit(f"{bundle!r} is not in this manifest")

        for idx, info in tree["AssetBundleInfos"]:
            if idx != target:
                continue
            was = "".join(
                f"{info['AssetBundleHash'][f'bytes[{k}]']:02x}" for k in range(16)
            )
            for k in range(16):
                info["AssetBundleHash"][f"bytes[{k}]"] = raw[k]
            obj.save_typetree(tree)
            # packer="original": the default writes the manifest uncompressed.
            out = env.file.save(packer="original")
            with open(dst, "wb") as fh:
                fh.write(out)
            _assert_written(dst, bundle, new_hash)
            return was, hashlib.md5(out).hexdigest()

    raise SystemExit("no AssetBundleManifest object found")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("src")
    ap.add_argument("dst")
    ap.add_argument("bundle", help="logical bundle name, e.g. ui_textures_ui_logo_sab")
    ap.add_argument("new_hash", help="32 hex chars")
    args = ap.parse_args()

    was, digest = repoint(args.src, args.dst, args.bundle, args.new_hash)
    print(f"{args.bundle}: {was} -> {args.new_hash}")
    print(f"wrote {args.dst} (md5 {digest})")


if __name__ == "__main__":
    main()
