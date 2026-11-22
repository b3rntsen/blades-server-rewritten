#!/usr/bin/env python3
"""Regenerate blades_lib/src/decoration_craft_prestige.json.

DecorationCrafting recipe id -> the town prestige (townXp) collecting that craft
pays. The number is the decoration's own, and the APK states it twice:

  * `PropDataList` (bundle `common`): every decoration's `PropData`
    carries `_txpReward`. This is the number the client's Workshop screen prints
    as the craft's prestige reward (`CraftingMenuCraftDetailsPanel.SetItem` ->
    `PropDataList.GetPropDataByUid` -> `_txpReward`, text key
    `UI.EditSelectableMenu.PrestigeReward.Body`). All 141 DecorationCrafting
    outputs have one and none is 0 (report #597).
  * `ItemBundleGenerationData.town_xp` on the bundle that sells exactly one unit
    of the decoration and nothing else: `Workshop - ...` (39 decorations) or the
    gem store's `IAP_Deco_...` (35, report #538). A Workshop bundle wins over an
    IAP_Deco one; no decoration has both. Multi-item bundles (level-up offers,
    chests) are ignored: their `town_xp` prices the whole bundle.

The two agree on 72 of the 74 decorations that have both. Where they differ
(Unicorn Fountain 127 / 142, Imperial Bath 24 / 89) the larger one is paid, so
nothing this table paid before #597 went down.

Usage -- the props either straight from the bundles (needs UnityPy >= 1.25;
`common` carries embedded typetrees) or from a `read_typetree()` dump
of the PropDataList MonoBehaviour written as JSON:

  script/extract_decoration_craft_prestige.py \
      <capture-repo>/reference/game-defs/shop_bundles.json \
      <capture-repo>/reference/game-defs/items.json \
      --bundles <unzipped-apk>/assets/Bundles > blades_lib/src/decoration_craft_prestige.json

  script/extract_decoration_craft_prestige.py shop_bundles.json items.json \
      --props-json PropDataList.json > blades_lib/src/decoration_craft_prestige.json
"""
import argparse
import os
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def load_props(bundles_dir: str | None, props_json: str | None) -> dict[str, tuple[str, int]]:
    """Decoration item template id -> (PropData editor name, `_txpReward`)."""
    if props_json:
        tree = json.load(open(props_json))
    else:
        import UnityPy

        tree = None
        env = UnityPy.load(os.path.join(bundles_dir, "common"))
        for o in env.objects:
            if o.type.name != "MonoBehaviour":
                continue
            try:
                if o.read().m_Script.read().m_ClassName == "PropDataList":
                    tree = o.read_typetree()
                    break
            except Exception:
                continue
        if tree is None:
            sys.exit(f"no PropDataList in {bundles_dir}/common")
    return {
        p["_itemTemplateID"]["_uid"]["_id"]: (p["_editorName"], int(p["_txpReward"]))
        for p in tree["_templateList"]
    }


def main(bundles_path: str, items_path: str, props: dict[str, tuple[str, int]]) -> None:
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
    decorations = 0
    for recipe_id, r in sorted(recipes.items()):
        if r.get("craftingType") != "DecorationCrafting":
            continue
        decorations += 1
        template = r["outputItemTemplateId"]
        entry = {
            "outputItemTemplateId": template,
            "name": items.get(template, {}).get("name", r.get("name")),
            "townXp": 0,
        }
        prop = props.get(template)
        if prop:
            entry["prop"], entry["propTxpReward"] = prop
            entry["townXp"] = prop[1]
        found = by_template.get(template)
        if found:
            best = min(rank for rank, _ in found)
            values = {b["town_xp"] for rank, b in found if rank == best}
            if len(values) != 1:
                sys.exit(f"{recipe_id}: bundles disagree on town_xp {sorted(values)}")
            bundle = sorted((b for rank, b in found if rank == best), key=lambda b: b["uuid"])[0]
            entry["bundleTownXp"] = values.pop()
            entry["bundleId"] = bundle["uuid"]
            entry["bundle"] = bundle["editor_name"]
            entry["townXp"] = max(entry["townXp"], entry["bundleTownXp"])
        if entry["townXp"] > 0:
            out[recipe_id] = entry

    both = [e for e in out.values() if "propTxpReward" in e and "bundleTownXp" in e]
    meta = {
        "description": "DecorationCrafting recipe id -> the town prestige (townXp) collecting that craft pays.",
        "source": "APK PropDataList (bundle common): each decoration's PropData._txpReward, the number the client's Workshop screen shows as the craft's prestige reward (propTxpReward). Also APK ItemBundleGenerationDataList (reference/game-defs/shop_bundles.json in the capture repo): the single-item, quantity-1 bundle selling the recipe's output -- a Workshop bundle, else the gem store's IAP_Deco_ bundle -- carries `town_xp` (bundleTownXp). townXp is the larger of the two; recipe -> output is recipe_outputs.json. Regenerate with script/extract_decoration_craft_prestige.py.",
        "evidence": "Retail capture 17253 (2026-05-15): collecting DecorationCrafting recipe 6d164b3d (Ancient Tree) moved town.levelInfo.experiencePoints 87565 -> 87656, +91 = its PropData._txpReward = its Workshop bundle's town_xp; the response carried `town`. No other retail capture collects a decoration craft.",
        "agreement": f"{sum(1 for e in both if e['propTxpReward'] == e['bundleTownXp'])} of the {len(both)} decorations with both a PropData and a single-decoration bundle carry the same number in each; the rest keep the larger.",
        "not_here": f"{decorations - len(out)} of the {decorations} DecorationCrafting recipes pay nothing.",
    }
    json.dump({"_meta": meta, "recipes": out}, sys.stdout, indent=1, ensure_ascii=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("shop_bundles")
    ap.add_argument("items")
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--bundles", help="unzipped APK assets/Bundles directory")
    src.add_argument("--props-json", help="read_typetree() dump of the PropDataList MonoBehaviour")
    args = ap.parse_args()
    main(args.shop_bundles, args.items, load_props(args.bundles, args.props_json))
