#!/usr/bin/env python3
"""Replace or overlay one named Texture2D inside a shipped AssetBundle.

The bundles behind ``bundles.blades.bgs.services`` are plain, unencrypted UnityFS
and nothing in the channel is signed (``"cert": false`` on all 198 manifest
versions), so a re-skin is an ordinary file edit. This is the smallest tool that
does it.

    ./patch_bundle_texture.py IN OUT --texture UI_Logo_Blades_White --overlay pumpkin.png

Two things to know before using it in anger:

* **Compression.** ``BundleFile.save()`` defaults to ``packer=None``, which is
  *no block compression* — that, not texture re-encoding, is why a naive
  round-trip inflates a bundle (``ui_textures_ui_logo_sab``: 1,200,205 bytes in,
  1,722,289 out). Both tools pass ``packer="original"``, which brings the same
  edit to 1,222,233 — +1.8% rather than +43%. Every byte here is on the download
  path for every player.
* **The hash is a cache key, not a checksum.** For clients to pick the new file
  up you must also point the manifest's ``AssetBundleHash`` for this bundle at a
  new value (see ``repoint_manifest.py``). Leaving it alone means only clients
  with a cold cache ever see the change.

See ``docs/asset-reskin-feasibility.md`` for what is settled and what is not —
notably the ``GetBundleCRC`` question, which only an on-device run answers.
"""

import argparse
import hashlib
import os

import UnityPy
from PIL import Image


def patch(src, dst, texture, overlay=None, replace=None, pos="bottom-right", margin=8):
    """Write ``src`` to ``dst`` with ``texture`` overlaid or replaced.

    Returns ``(md5, size)``. The md5 is a convenient new Hash128 — any 16-byte
    value works, and deriving it from the content keeps the step reproducible.
    """
    if os.path.abspath(src) == os.path.abspath(dst):
        raise SystemExit("src and dst are the same file; refusing to edit in place")
    env = UnityPy.load(src)
    for obj in env.objects:
        if obj.type.name != "Texture2D":
            continue
        data = obj.read()
        if data.m_Name != texture:
            continue

        img = data.image.convert("RGBA")
        if replace:
            new = Image.open(replace).convert("RGBA").resize(img.size)
        else:
            new = img.copy()
            ov = Image.open(overlay).convert("RGBA")
            # An overlay bigger than the base would give a negative offset and
            # PIL would raise; scale it to fit instead.
            if ov.width + 2 * margin > new.width or ov.height + 2 * margin > new.height:
                ov.thumbnail((new.width - 2 * margin, new.height - 2 * margin))
            x = margin if "left" in pos else new.width - ov.width - margin
            y = margin if "top" in pos else new.height - ov.height - margin
            new.alpha_composite(ov, (x, y))

        data.image = new
        data.save()
        # packer="original" keeps the bundle's own block compression. The default
        # (None) writes it uncompressed and inflates the download by ~43%.
        out = env.file.save(packer="original")
        with open(dst, "wb") as fh:
            fh.write(out)
        return hashlib.md5(out).hexdigest(), len(out)

    raise SystemExit(f"no Texture2D named {texture!r} in {src}")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("src", help="bundle taken from the mirror, named by its hash")
    ap.add_argument("dst")
    ap.add_argument("--texture", required=True, help="m_Name of the Texture2D to change")
    grp = ap.add_mutually_exclusive_group(required=True)
    grp.add_argument("--overlay", help="PNG composited onto the existing texture")
    grp.add_argument("--replace", help="PNG that replaces the texture wholesale")
    # --pos only positions an overlay; --replace covers the whole texture.
    ap.add_argument(
        "--pos",
        default="bottom-right",
        choices=["top-left", "top-right", "bottom-left", "bottom-right"],
    )
    args = ap.parse_args()

    digest, size = patch(
        args.src, args.dst, args.texture, args.overlay, args.replace, args.pos
    )
    print(f"wrote {args.dst}  {size:,} bytes")
    print(f"md5 (use as the new Hash128): {digest}")


if __name__ == "__main__":
    main()
