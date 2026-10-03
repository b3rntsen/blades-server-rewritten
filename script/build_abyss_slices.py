#!/usr/bin/env python3
"""Write `abyssSlices` into deploy/static/abyss.json — retail's Abyss generation
table, the client's own `AbyssData` list of `AbyssSlice` ScriptableObjects (#294).

WHY. Deep floors were picked from a DESIGNED table (`depthBands` x guessed
`monsterTiers`) and, past floor 150, by cycling the 15-dungeon `randomPool`.
Floors 80-149 came out a third Liches Outcast Nether and a quarter dragons;
floors 150+ only Atronach and Dremora. Retail's 26 captured runs carry the
dungeon of all 150 floors they pre-generated (3,900 slices): from floor 80 up
it is 33 dungeons in near-equal shares — Dremora, Atronach and mixed
Atronach/Dremora ~56%, Skeletons ~14%, Liches Outcast Nether ~12%, Liches ~10%,
Warlord ~7%, no dragons at all.

Each `AbyssSlice` names a dungeon, a `_levelRange` and a `_requiredQuestPointer`
and a `_randomWeight`. All 3,900 captured slices are a listed dungeon served at a
difficultyLevel inside its slice's level range; runs of characters that had not
finished a slice's required quest never got it (the level 4-38 runs: none of
the 18 quest-gated deep dungeons in 640 deep floors).

SOURCE. blades-capture `reference/game-defs/abyss.json` `slices` — an
extraction of the APK's AbyssSlice assets (161 of them, every one referenced by
`AbyssData`). Usage:

    python3 script/build_abyss_slices.py <blades-capture>/reference/game-defs/abyss.json
"""
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TARGET = ROOT / "deploy" / "static" / "abyss.json"


def main(source: str) -> None:
    slices = json.loads(pathlib.Path(source).read_text())["slices"]
    rows = []
    for s in sorted(slices.values(), key=lambda s: s["name"]):
        rows.append({
            "name": s["name"],
            "dungeonSettingsId": s["dungeon_settings_uid"],
            "minLevel": s["min_level"],
            "maxLevel": s["max_level"],
            "randomWeight": s["random_weight"],
            "requiredQuestId": s["required_quest_uid"],
        })
    doc = json.loads(TARGET.read_text())
    doc["abyssSlices"] = rows
    TARGET.write_text(json.dumps(doc, indent=2) + "\n")
    print(f"wrote {len(rows)} abyssSlices to {TARGET}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
