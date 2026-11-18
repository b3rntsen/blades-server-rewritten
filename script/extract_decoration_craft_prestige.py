#!/usr/bin/env python3
"""Regenerate blades_lib/src/decoration_craft_prestige.json.

DecorationCrafting recipe id -> the town prestige (townXp) collecting that craft
pays. The number is the decoration's own: the `town_xp` on the APK
ItemBundleGenerationData that sells exactly one unit of the recipe's output and
nothing else. Two such bundle families exist:

  * `Workshop - ...`  the Workshop's own decoration bundles (39 decorations);
  * `IAP_Deco_...`    the gem store's single-decoration bundles (report #538).

A Workshop bundle wins over an IAP_Deco one; no decoration has both. Multi-item
bundles (level-up offers, chests) are ignored: their `town_xp` prices the whole
bundle, not the decoration. A recipe whose output has neither pays nothing.

Usage:
  script/extract_decoration_craft_prestige.py \
      <capture-repo>/reference/game-defs/shop_bundles.json \
      <capture-repo>/reference/game-defs/items.json > blades_lib/src/decoration_craft_prestige.json
"""
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def main(bundles_path: str, items_path: str) -> None:
    bundles = json.load(open(bundles_path))
    items = json.load(open(items_path))
    recipes = json.load(open(ROOT / "blades_lib/src/recipe_outputs.json"))["recipes"]

    by_template: dict[str, list[tuple[int, dict]]] = {}
    for b in bundles.values():
        its = b.get("items") or []
        if len(its) != 1 or its[0].get("quantity") != 1 or not b.get("town_xp"):
            continue
        name = b.get("editor_name") or ""
        if name.startswith("Workshop - "):
            rank = 0
        elif name.startswith("IAP_Deco_"):
            rank = 1
        else:
            continue
        by_template.setdefault(its[0]["template_uuid"], []).append((rank, b))

    out = {}
    for recipe_id, r in sorted(recipes.items()):
        if r.get("craftingType") != "DecorationCrafting":
            continue
        found = by_template.get(r["outputItemTemplateId"])
        if not found:
            continue
        best = min(rank for rank, _ in found)
        values = {b["town_xp"] for rank, b in found if rank == best}
        if len(values) != 1:
            sys.exit(f"{recipe_id}: bundles disagree on town_xp {sorted(values)}")
        bundle = sorted((b for rank, b in found if rank == best), key=lambda b: b["uuid"])[0]
        out[recipe_id] = {
            "outputItemTemplateId": r["outputItemTemplateId"],
            "name": items.get(r["outputItemTemplateId"], {}).get("name", r.get("name")),
            "townXp": values.pop(),
            "bundleId": bundle["uuid"],
            "bundle": bundle["editor_name"],
        }

    meta = {
        "description": "DecorationCrafting recipe id -> the town prestige (townXp) collecting that craft pays.",
        "source": "APK ItemBundleGenerationDataList (reference/game-defs/shop_bundles.json in the capture repo): the single-item, quantity-1 bundle selling the recipe's output -- a Workshop bundle, else the gem store's IAP_Deco_ bundle -- carries `town_xp`; recipe -> output is recipe_outputs.json. Regenerate with script/extract_decoration_craft_prestige.py.",
        "evidence": "Retail capture 17253 (2026-05-15): collecting DecorationCrafting recipe 6d164b3d (Ancient Tree) moved town.levelInfo.experiencePoints 87565 -> 87656, +91 = its Workshop bundle's town_xp; the response carried `town`. The IAP_Deco_ values are what the retail store paid for buying the same decoration (report #538); no retail capture crafts one.",
        "not_here": f"The other {sum(1 for r in recipes.values() if r.get('craftingType') == 'DecorationCrafting') - len(out)} DecorationCrafting recipes' outputs have no single-decoration bundle and no prestige value anywhere in the APK data we hold, so they pay nothing rather than an invented number.",
    }
    json.dump({"_meta": meta, "recipes": out}, sys.stdout, indent=1, ensure_ascii=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main(*sys.argv[1:3])
