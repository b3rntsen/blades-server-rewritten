#!/usr/bin/env python3
"""Emit server/data/economy_audit_apk.json: the APK facts the economy audit
(`server/src/economy_audit.rs`) checks generated loot and the storefront against.

WHY (#392). HauDrauf: "check everything for logic, not on the few data points."
The audit samples every shop product and chest generator and checks the output
against what the GAME allows, so it needs two APK tables the server itself never
loads:

* `itemRequiredLevel` -- `ItemTemplate.requiredLevel` for every gear, shield,
  ring and necklace template (the level the gear UI shows). From
  reference/game-defs/items.json in blades-capture (x_items.py over the APK).
* `offerLevelGates` -- `GlobalShopProduct._minPlayerLevelAllowedForPurchase` /
  `_maxPlayerLevelAllowedForPurchase` for every catalogue product. The client
  hides an offer outside that range; the server's catalogue does not carry it.
  `reference/game-defs/shop_offers.json` keeps it only for the 295 APK-active
  offers (27 of the 546 we serve), so it is read from the APK here.

Nothing is authored. Usage (venv and APK extract as in
blades-capture/reference/game-defs/README.md):

    APK_EXTRACT=/tmp/blades-apk-extract \\
      python extract_economy_audit_apk.py <game-defs dir> <out.json>
"""
import json
import os
import sys

GEAR_TYPES = {"weapon", "armor", "shield", "ring", "jewelry"}


def offer_gates(game_defs: str) -> dict:
    sys.path.insert(0, os.path.join(game_defs, "extract"))
    import lib  # noqa: E402  (blades-capture reference/game-defs/extract/lib.py)

    _, objs = lib.load()
    for o in objs:
        if o.type.name != "MonoBehaviour" or lib.classname(o) != "GlobalShopProductsCatalog":
            continue
        out = {}
        for p in o.read_typetree()["_templateList"]:
            uid = p["_uid"]["_id"] if isinstance(p["_uid"], dict) else p["_uid"]
            out[str(uid).lower()] = {
                "editorName": p.get("_editorName"),
                "min": p.get("_minPlayerLevelAllowedForPurchase", 0),
                "max": p.get("_maxPlayerLevelAllowedForPurchase", 0),
            }
        return out
    sys.exit("no GlobalShopProductsCatalog in the APK bundles")


def main(game_defs: str, out_path: str) -> None:
    items = json.load(open(os.path.join(game_defs, "items.json")))
    required = {
        t: v["requiredLevel"]
        for t, v in sorted(items.items())
        if v.get("type_name") in GEAR_TYPES and "requiredLevel" in v
    }
    gates = offer_gates(game_defs)
    out = {
        "_meta": {
            "description": "APK facts for the economy audit: gear requiredLevel and global-shop level gates.",
            "source": "APK sha256 fd6e55f5...; items via blades-capture reference/game-defs/items.json, "
            "gates via GlobalShopProductsCatalog._templateList",
            "generator": "script/extract_economy_audit_apk.py",
            "gateSemantics": "min/max 0/0 = no gate; otherwise the client lists the offer only for min <= level <= max",
        },
        "itemRequiredLevel": required,
        "offerLevelGates": dict(sorted(gates.items())),
    }
    with open(out_path, "w") as f:
        json.dump(out, f, indent=1)
        f.write("\n")
    print(f"wrote {len(required)} templates and {len(gates)} offer gates to {out_path}")


if __name__ == "__main__":
    main(*sys.argv[1:3])
