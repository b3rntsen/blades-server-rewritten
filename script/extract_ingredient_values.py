#!/usr/bin/env python3
"""Extract retail's gem price for MISSING craft ingredients into
blades_lib/src/ingredient_values.json.

Why this exists (tracker #226)
------------------------------
When a player is short of an enchant's gold or materials the client offers to
"spend gems on the missing resources" and sends `gemsPayment: true`. The server
refused that with a 400 (it had no gem price for a missing resource), and the
client restarts on that 400. Retail used what the player had and billed the
shortfall in gems. The price is client data, from the il2cpp dump:

    IngredientValueTable (dump.cs:548119, one asset, 72 entries)
        List<IngredientValueMapping> _ingredientValues
            _item      UidItemTemplatePointer   the ingredient (gold is one of them)
            _gemValue  float                    gems per unit
    int GetGemCostForIngredient(Uid ingredient, int quantity)   RVA 0x1E5C2CC
        = (int)ceil((float)quantity * _gemValue)   (fmul s, frintp s, fcvtzs)

Identity check against retail (2026-06-07 snapshot, captures 59598 and 59601):
gold 0.015/unit — 8467 missing gold cost 128 gems, 35000 cost 525.

Reproduce
---------
  unzip reference/apk/blades.apk \\
      'assets/Bundles/*' 'lib/arm64-v8a/libil2cpp.so' \\
      'assets/bin/Data/Managed/Metadata/global-metadata.dat' -d /tmp/blades-apk-extract
  pip install 'UnityPy==1.25.0' 'TypeTreeGeneratorAPI==0.0.10'
  APK_EXTRACT=/tmp/blades-apk-extract python3 script/extract_ingredient_values.py

APK: reference/apk/blades.apk in the blades-capture repo, SHA-256
fd6e55f561542cac41a016975318bc69759e512d5155b3c8945367883c833417. Same method as
`script/extract_enchanting_data.py`.
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


def load_tables():
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
    found = []
    for o in objs:
        if o.type.name != "MonoBehaviour":
            continue
        try:
            if o.read().m_Script.read().m_ClassName == "IngredientValueTable":
                found.append(o.read_typetree())
        except Exception:
            continue
    return found


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="blades_lib/src/ingredient_values.json")
    args = ap.parse_args()
    tables = load_tables()
    if len(tables) != 1:
        sys.exit("expected exactly one IngredientValueTable, found %d" % len(tables))
    values = {}
    names = {}
    for m in tables[0]["_ingredientValues"]:
        tid = m["_item"]["_uid"]["_id"]
        if tid in values:
            sys.exit("duplicate ingredient %s" % tid)
        values[tid] = m["_gemValue"]
        names[tid] = m["_editorName"]
    out = {
        "_meta": {
            "source": "reference/apk/blades.apk (blades-capture), sha256 " + APK_SHA256,
            "asset": "IngredientValueTable._ingredientValues",
            "rule": "gems = ceil(f32(quantity) * f32(gemValue)), per ingredient "
            "(IngredientValueTable.GetGemCostForIngredient, RVA 0x1E5C2CC)",
            "generator": "script/extract_ingredient_values.py",
            "names": names,
        },
        "gemValues": values,
    }
    with open(args.out, "w") as f:
        json.dump(out, f, indent=1)
        f.write("\n")
    print("wrote %d ingredient values to %s" % (len(values), args.out))


if __name__ == "__main__":
    main()
