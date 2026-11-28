#!/usr/bin/env python3
"""Raise the backpack cap by appending inventory-level rows to ``InventoryData``.

The cap is a CLIENT decision. `InventoryData` lives in the ``common`` bundle and
the client computes, in `PlayerActor.GetPlayerInventoryCapacity`:

    capacity = _initialPlayerInventorySize
             + _staminaLevelExtraSlots[staminaLevel]
             + GetInventoryCapacity(character.inventoryLevel)

Stock that is ``40 + 0 + 80 = 120`` at the highest authored level, 8. The server
never checks capacity at all, so nothing there can raise it.

Disassembly of `GetInventoryCapacity` (libil2cpp.so, RVA 0x1DAC0D8) settles the
two things that make this safe to ship:

* ``_capacity`` is **absolute**, not a per-level increment — the loop *assigns*
  ``entry._capacity`` for the highest row whose ``_level <= inventoryLevel``.
* an inventoryLevel with no matching row **clamps down** to the highest row that
  exists; it does not fall through to zero.

So a client WITHOUT this pack, handed ``inventoryLevel = 9``, keeps the stock 120
slots. The two gates — who gets the pack, and whose character carries the new
level — are therefore independent, and neither one alone changes anything for
anybody else.

    ./patch_inventory_capacity.py bisect/common.orig common.invcap --capacity 9959
"""
import argparse
import hashlib
import os
import sys

APK = os.environ.get("APK_EXTRACT", "/tmp/blades-apk-extract")
#: TypeTreeGeneratorAPI must be importable alongside UnityPy (see
#: requirements.txt); the il2cpp files come from your own APK (APK_EXTRACT).

import UnityPy  # noqa: E402
from UnityPy.helpers.TypeTreeGenerator import TypeTreeGenerator  # noqa: E402


def _generator(env, obj):
    g = TypeTreeGenerator(obj.assets_file.unity_version)
    with open(os.path.join(APK, "lib/arm64-v8a/libil2cpp.so"), "rb") as so, \
         open(os.path.join(APK, "assets/bin/Data/Managed/Metadata/global-metadata.dat"), "rb") as md:
        g.load_il2cpp(so.read(), md.read())
    env.typetree_generator = g
    return g


def patch(src, dst, level, capacity, cost):
    env = UnityPy.load(src)
    objs = list(env.objects)
    if not objs:
        raise SystemExit("%s holds no objects — wrong file?" % src)
    _generator(env, objs[0])

    target = None
    for obj in objs:
        if obj.type.name != "MonoBehaviour":
            continue
        try:
            tree = obj.read_typetree()
        except Exception:
            continue
        if isinstance(tree, dict) and "_playerInventoryLevelList" in tree:
            target = (obj, tree)
            break
    if target is None:
        raise SystemExit("no InventoryData in %s" % src)

    obj, tree = target
    rows = tree["_playerInventoryLevelList"]
    if not rows:
        raise SystemExit("InventoryData has no level rows — refusing to guess the shape")
    if any(r["_level"] == level for r in rows):
        raise SystemExit("level %d already exists — pick one past %d"
                         % (level, max(r["_level"] for r in rows)))

    base = tree["_initialPlayerInventorySize"]
    top = max(rows, key=lambda r: r["_level"])
    row = dict(top)
    row["_level"] = level
    row["_capacity"] = capacity
    row["_cost"] = cost
    rows.append(row)
    rows.sort(key=lambda r: r["_level"])

    obj.save_typetree(tree)
    data = env.file.save(packer="original")
    with open(dst, "wb") as fh:
        fh.write(data)

    digest = hashlib.md5(data).hexdigest()
    print("  stock  level %d -> %d slots" % (top["_level"], base + top["_capacity"]))
    print("  patched level %d -> %d slots (base %d + %d)"
          % (level, base + capacity, base, capacity))
    print("  %s  %d bytes  md5 %s" % (dst, len(data), digest))
    return digest, len(data)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("src", help="stock bundle, e.g. bisect/common.orig")
    ap.add_argument("dst", help="where to write the patched bundle")
    ap.add_argument("--level", type=int, default=9, help="new inventory level (default 9)")
    ap.add_argument("--capacity", type=int, default=9959,
                    help="slots this level grants ON TOP of _initialPlayerInventorySize")
    ap.add_argument("--cost", type=int, default=2000000000,
                    help="in-game price of the upgrade; the default is deliberately "
                         "unaffordable so the row cannot be bought by accident")
    args = ap.parse_args()
    patch(args.src, args.dst, args.level, args.capacity, args.cost)


if __name__ == "__main__":
    main()
