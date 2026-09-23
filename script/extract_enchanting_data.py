#!/usr/bin/env python3
"""Extract retail's enchanting tables from the APK into deploy/static/enchanting.json.

Why this exists
---------------
`item_mod_recipes.json` holds the enchant OUTCOMES we happened to capture (19 enchant
recipes, 83 distinct outcomes), and the server used to hand every item one of them
chosen by `item_id % outcomes.len()`: the same item always got the same enchant, and
the other 183 enchant recipes the client offers did nothing at all. Retail rolled.
The roll's inputs ship inside the APK. From the il2cpp dump:

    ItemModifierRecipe  (m_Name "EnchantingRecipeList", 202 recipes)
        _filterTypes / _filterSlots / _filterWeaponClass / _filterWeaponType
        _inputs   gold + soul gem + materials
        _outputs  [{_itemProperty, _tier, _odds}]      the PRIMARY enchantment
        _duration seconds
    SecondaryEnchantmentEffectData (dump.cs:558288, 16 tables)
        _itemType, _weaponType, _equipmentSlot         which items the table serves
        float[] _oddsOfAdditionalEnchantment           P(0), P(1), P(2) secondaries
        ItemSecondaryPropertyData[] _secondaryProperties  {_property, _odds}  weights
    ArcaneTier (dump.cs:550117, ArcaneTierCatalog)
        List<float> _oddsOfAdditionalEnchantment       tier 1 [0,1,0], tier 2 [0,0,1]
        loc keys ARCANE_ITEM_GUARANTEED_1_ENCHANTMENT / _2_ENCHANTMENTS

so an enchant is: the recipe's primary property at the recipe's tier, plus N secondary
properties, N drawn from the item's arcane tier when it has one and from the item
type's table otherwise, each secondary a weighted draw WITHOUT replacement from that
table's pool. Identity checks against the captured retail enchants (37 jobs in the
2026-06-07 snapshot + the 83 outcomes in item_mod_recipes.json) are in the server's
craft.rs tests and are printed by this script.

Outputs
-------
  enchanting.json  {"recipes": {recipeId: {...}}, "secondaryTables": [...],
                    "arcaneTierCountOdds": {"1": [...], ...},
                    "templates": {itemTemplateId: tableIndex}, "_meta": {...}}

Reproduce
---------
  unzip reference/apk/blades.apk \\
      'assets/Bundles/*' 'lib/arm64-v8a/libil2cpp.so' \\
      'assets/bin/Data/Managed/Metadata/global-metadata.dat' -d /tmp/blades-apk-extract
  pip install 'UnityPy==1.25.0' 'TypeTreeGeneratorAPI==0.0.10'
  APK_EXTRACT=/tmp/blades-apk-extract python3 script/extract_enchanting_data.py \\
      --out deploy/static

APK: reference/apk/blades.apk in the blades-capture repo, Unity 2019.4.37f1, il2cpp
metadata v24, SHA-256 fd6e55f561542cac41a016975318bc69759e512d5155b3c8945367883c833417.
Same extraction method as `script/extract_item_repair_data.py`.
"""

import argparse
import json
import os
import sys

APK_EXTRACT = os.environ.get("APK_EXTRACT", "/tmp/blades-apk-extract")
BUNDLES = APK_EXTRACT + "/assets/Bundles"
SO = APK_EXTRACT + "/lib/arm64-v8a/libil2cpp.so"
META = APK_EXTRACT + "/assets/bin/Data/Managed/Metadata/global-metadata.dat"
APK_SHA256 = "fd6e55f561542cac41a016975318bc69759e512d5155b3c8945367883c833417"

# BGS.Game.Inventory.ItemType / ItemEquipmentSlot (dump.cs:559327, 559350).
WEAPON, ARMOR, SHIELD, RING, NECKLACE = 2, 3, 9, 10, 11
SLOT_NECK, SLOT_FINGER = 8, 9
# Jewellery templates carry no `_equipmentSlot`; the slot is implied by the type.
IMPLIED_SLOT = {RING: SLOT_FINGER, NECKLACE: SLOT_NECK}

TEMPLATE_LISTS = (
    "WeaponTemplateList",
    "ArmorTemplateList",
    "ShieldTemplateList",
    "ItemTemplateList",
)


def load_objects():
    import UnityPy
    from UnityPy.helpers.TypeTreeGenerator import TypeTreeGenerator

    for p in (BUNDLES, SO, META):
        if not os.path.exists(p):
            sys.exit("missing %s — see the module docstring for the unzip command" % p)
    env = UnityPy.load(BUNDLES)
    objs = list(env.objects)
    gen = TypeTreeGenerator(objs[0].assets_file.unity_version)
    gen.load_il2cpp(open(SO, "rb").read(), open(META, "rb").read())
    env.typetree_generator = gen
    by_class = {}
    for o in objs:
        if o.type.name != "MonoBehaviour":
            continue
        try:
            cn = o.read().m_Script.read().m_ClassName
        except Exception:
            continue
        if cn in ("ItemModifierRecipeList", "SecondaryEnchantmentEffectDataList",
                  "ArcaneTierCatalog", "ItemPropertyList") + TEMPLATE_LISTS:
            try:
                by_class.setdefault(cn, []).append(o.read_typetree())
            except Exception:
                continue
    return by_class


def uid(d):
    inner = d.get("_uid", d) if isinstance(d, dict) else None
    i = inner.get("_id") if isinstance(inner, dict) else None
    return None if i in (None, "", "0") else i


def table_matches(t, item_type, weapon_type, slot):
    return (
        t["itemType"] == item_type
        and t["weaponType"] in (0, weapon_type)
        and t["equipmentSlot"] in (0, slot)
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="deploy/static")
    args = ap.parse_args()
    c = load_objects()

    enchant_lists = [l for l in c["ItemModifierRecipeList"] if l.get("m_Name") == "EnchantingRecipeList"]
    if len(enchant_lists) != 1:
        sys.exit("expected exactly one EnchantingRecipeList, found %d" % len(enchant_lists))
    recipes = {}
    for r in enchant_lists[0]["_recipes"]:
        outs = r["_outputs"]
        if len(outs) != 1 or abs(outs[0]["_odds"] - 1.0) > 1e-6:
            sys.exit("recipe %s has a non-trivial output list; the model assumes one" % uid(r))
        # `_inputs` is what starting the enchant costs: gold (a currency template) plus
        # the soul gem and materials (stackable templates), in authored order.
        inputs = []
        for i in r.get("_inputs") or []:
            tpl = uid(i.get("_itemTemplate"))
            qty = int(i.get("_quantity") or 0)
            if not tpl or qty <= 0:
                sys.exit("recipe %s has an input without a template or quantity" % uid(r))
            inputs.append({"templateId": tpl, "quantity": qty})
        recipes[uid(r)] = {
            "name": r["_name"]["_key"],
            "property": uid(outs[0]["_itemProperty"]),
            "tier": outs[0]["_tier"],
            "durationMs": int(round(r["_duration"] * 1000)),
            "inputs": inputs,
            "filterTypes": r["_filterTypes"],
            "filterSlots": r["_filterSlots"],
            "filterWeaponClass": r["_filterWeaponClass"],
            "filterWeaponType": r["_filterWeaponType"],
        }

    tables = []
    for t in c["SecondaryEnchantmentEffectDataList"][0]["_templateList"]:
        odds = [round(x, 6) for x in t["_oddsOfAdditionalEnchantment"]]
        tables.append({
            "itemType": t["_itemType"],
            "weaponType": t["_weaponType"],
            "equipmentSlot": t["_equipmentSlot"],
            "countOdds": odds,
            # Zero-weight entries are kept: they are authored, and dropping them would
            # make the file disagree with the APK for no gain (the roll never picks them).
            "properties": [
                {"id": uid(p["_property"]), "weight": round(p["_odds"], 6)}
                for p in t["_secondaryProperties"]
            ],
        })

    arcane = {}
    for a in c["ArcaneTierCatalog"][0]["_arcaneTierList"]:
        arcane[str(a["_tier"])] = [round(x, 6) for x in a["_oddsOfAdditionalEnchantment"]]

    templates = {}
    unmatched = 0
    for cn in TEMPLATE_LISTS:
        for lst in c.get(cn, []):
            for t in lst.get("_templateList", []):
                item_type = t.get("_type")
                if item_type not in (WEAPON, ARMOR, SHIELD, RING, NECKLACE):
                    continue
                slot = t.get("_equipmentSlot", IMPLIED_SLOT.get(item_type, 0))
                wt = t.get("_weaponType", 0)
                hits = [i for i, tb in enumerate(tables) if table_matches(tb, item_type, wt, slot)]
                if len(hits) > 1:
                    sys.exit("template %s matches %d tables" % (uid(t), len(hits)))
                if hits:
                    templates[uid(t)] = hits[0]
                else:
                    unmatched += 1

    # Sanity: every secondary property must carry every tier a recipe can ask for,
    # since a secondary takes the primary's tier (see craft.rs).
    tiers_of = {}
    for lst in c["ItemPropertyList"]:
        for p in lst["_propertyList"]:
            tiers_of[uid(p)] = sorted(x["_tierNumber"] for x in p["_tiers"])
    missing_tier = sum(
        1 for tb in tables for p in tb["properties"] if p["weight"] > 0
        for r in recipes.values() if r["tier"] not in tiers_of.get(p["id"], [])
    )

    out = {
        "_meta": {
            "source": "reference/apk/blades.apk (blades-capture), sha256 " + APK_SHA256,
            "script": "script/extract_enchanting_data.py",
            "recipes": len(recipes),
            "secondaryTables": len(tables),
            "templates": len(templates),
            "templatesWithoutTable": unmatched,
        },
        "recipes": dict(sorted(recipes.items())),
        "secondaryTables": tables,
        "arcaneTierCountOdds": arcane,
        "templates": dict(sorted(templates.items())),
    }
    path = os.path.join(args.out, "enchanting.json")
    with open(path, "w") as f:
        json.dump(out, f, indent=1, sort_keys=False)
        f.write("\n")
    print("[enchanting] %d recipes, %d tables, %d templates (%d without a table), "
          "arcane tiers %s, (recipe, secondary) pairs missing the recipe's tier: %d -> %s"
          % (len(recipes), len(tables), len(templates), unmatched, sorted(arcane),
             missing_tier, path), file=sys.stderr)


if __name__ == "__main__":
    main()
