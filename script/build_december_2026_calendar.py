"""Build december_2026_calendar.json from HauDrauf's proposal (tracker #499, msg 3687)
plus the template ids re-derived from the game definitions and the APK's en-US / de-DE
localisation tables. Usage: build_calendar.py <items.json> <loc_all.json> <offer_contents.json> <shop_bundles.json> <out.json>"""
import json, sys
items = json.load(open(sys.argv[1]))
loc = json.load(open(sys.argv[2]))
offers = json.load(open(sys.argv[3]))["offers"]
bundles = json.load(open(sys.argv[4]))
OUT = sys.argv[5]

# Verbatim from message 3687.
PROPOSAL = ("1.12. 93/3,2. 19/2, 3. 66/3, 4. 7/1, 5. 45/3 6. 53/3 50/3, 7. 24/3, 8. 75/3, 9.45/3, "
            "10. 17/2, 11. 40/3, 12. 1/1,  13. 24/3, 14. 9/1, 15. 66/3, 16. 11/1, 17. 93/3, 18. 92/1, "
            "19. 40/3, 20. 24/3, 21. 66/3, 22. 40/3, 23. 4/1, 24. 52/3 51/3 49/3, 31. 92/1 93/3 45/3")
# Numbers -> English names exactly as sent to him in messages 3168 and 3172.
NUMBERED = {1: "Birdsong Tree", 4: "Dragon Snow Globe", 7: "Grand Saturalia Tree", 9: "Saturalia Grove",
            11: "Sylvan Fountain", 17: "Frozen Heart Fountain", 19: "Saturalia Tree", 24: "Amethyst Beacon",
            40: "Emerald Beacon", 45: "Garnet Beacon", 49: "Ice Elk", 50: "Ice Goat", 51: "Ice Khajiit",
            52: "Ice Mammoth", 53: "Ice Unicorn", 66: "Peridot Beacon", 75: "Small Fountain",
            92: "Large Elm Tree", 93: "Beacon"}
NEVER_OWNED = {11, 50}

def clean(s):
    return s.replace("\\u00AD", "").replace("­", "")

def by_name(name):
    hits = [(k, v) for k, v in items.items() if v.get("name") == name and v.get("type") == 4]
    assert len(hits) == 1, (name, hits)
    return hits[0]

import re
doors = []
tokens = re.findall(r"(\d+)\.(?:12\.)?\s*((?:\d+/\d+\s*)+)", PROPOSAL)
for day, body in tokens:
    day = int(day)
    decos = []
    for num, qty in re.findall(r"(\d+)/(\d+)", body):
        num, qty = int(num), int(qty)
        uuid, item = by_name(NUMBERED[num])
        key = item["name_key"]
        assert clean(loc["en-US"][key]) == NUMBERED[num]
        d = {"number": num, "itemTemplateId": uuid, "quantity": qty, "nameKey": key,
             "en": clean(loc["en-US"][key]), "de": clean(loc["de-DE"][key])}
        if num in NEVER_OWNED:
            d["neverOwned"] = True
        decos.append(d)
    door = {"day": day, "date": "2026-12-%02d" % day, "decorations": decos,
            "pack": "incredibleTreasure" if day % 2 == 1 else "heroicEnchanter",
            "gems": {6: 600, 24: 1200}.get(day, 0), "armorSets": {6: 1, 24: 3}.get(day, 0)}
    doors.append(door)
assert [d["day"] for d in doors] == list(range(1, 25)) + [31], [d["day"] for d in doors]

def pack(offer_id):
    o = offers[offer_id]
    b = bundles[o["bundleId"]]
    names = {i["template_uuid"]: i["name"] for i in b["items"]}
    lines = [{"itemTemplateId": c["itemTemplateId"], "quantity": c["quantity"], "en": names[c["itemTemplateId"]]}
             for c in o["contents"]]
    assert {(l["itemTemplateId"], l["quantity"]) for l in lines} == {(i["template_uuid"], i["quantity"]) for i in b["items"]}
    order = [i["template_uuid"] for i in b["items"]]
    lines.sort(key=lambda l: order.index(l["itemTemplateId"]))
    return {"offerId": offer_id, "editorName": o["editorName"], "en": b["title"],
            "de": clean(loc["de-DE"][b["title_key"]]), "items": lines}

ARMOR = ["1063c3f9-1bbf-4962-b80d-fb9832075c1c", "2c9ac3da-4a90-4fcb-bb59-4202fdecffe0",
         "15f9963e-059c-464c-86ad-3a7eaa7cea9d", "0c2869fb-3a95-45c6-8dcc-706d43f0fab0"]
pieces = []
for u in ARMOR:
    it = items[u]
    assert it["editor_name"].startswith("Divine Dark Seducer") and it["type"] == 3 and it["tier"] == 10
    pieces.append({"itemTemplateId": u, "nameKey": it["name_key"], "editorName": it["editor_name"],
                   "en": clean(loc["en-US"][it["name_key"]]), "de": clean(loc["de-DE"][it["name_key"]])})

out = {
    "_meta": {
        "what": "The December 2026 gift calendar (tracker #499, HauDrauf's proposal, message 3687; approved by the owner 2026-11-23).",
        "decorations": "number = his numbered list (messages 3168/3172); itemTemplateId re-derived from the game definitions by English name; en/de are the APK's en-US/de-DE strings for nameKey.",
        "packs": "contents copied from deploy/static/global_shop_offer_contents.json (offerId), in the bundle's own order.",
        "armorSet": "the four Divine (tier 10, level 45) Dark Seducer pieces; the Divine set has no shield.",
        "neverOwned": "no player has ever held this decoration; check it on a device before its day."
    },
    "packs": {"incredibleTreasure": pack("c18127a3-57f0-4b5f-aa7d-54b3095e97ff"),
              "heroicEnchanter": pack("6ec8f67f-2cef-41aa-a7fc-f46237ae809c")},
    "armorSet": {"en": "Dark Seducer set (Divine)", "de": "Set der Dunklen Verführer (Göttlich)", "pieces": pieces},
    "doors": doors,
}
json.dump(out, open(OUT, "w"), ensure_ascii=False, indent=1)
print(len(doors), "doors;", sum(len(d["decorations"]) for d in doors), "decoration lines")
