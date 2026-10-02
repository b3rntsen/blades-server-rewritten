#!/usr/bin/env python3
"""Build blades_lib/src/abyss_kill_multipliers.json — the per-floor kill-score
multiplier the Abyss gauge needs (#312).

WHY. The client scores an Abyss kill as
    GetKillScore(enemy.Level - initialPlayerLevel) * enemy.KillScoreMultiplier
(AbyssController.NotifyEnemyKill, RVA 0x1C811D4: `fmul s8, s0, s1` on
EnemyKillRecord._killScoreMultiplier). The multiplier is 0.33 on critters
(skeevers, critter wolves/spiders/wisps), 1.0 on most enemies and 2.0 on
dragons and high-level liches/undead casters. The server scored every kill at
1.0, so in a critter floor it ran AHEAD of the client's gauge and paid rungs
the client had not reached; the client then set its previous-rung score past
its own score, the gauge stopped filling and the reward animation never fired
again (#312).

WHAT. Neither the wire nor parsed.json says which variant a spawn group holds,
so the multiplier is resolved per FLOOR: the dungeon's `monsters` tokens
(deploy/static/abyss.json dungeonPool) name its `* Family Abyss` families, each
family picks its variant by level band (enemies.json `bands`), and the floor
takes the LOWEST multiplier any candidate variant has at that level. Lowest,
because the two errors are not symmetric: a server behind the client leaves the
gauge full for a kill or two until the payment lands (the client resumes on it,
OnNewRewardInformationRecieved clears _waitingForNewReward); a server ahead of
it breaks the gauge for the rest of the run.

Usage:
    python3 script/build_abyss_kill_multipliers.py \
        <blades-capture>/reference/game-defs/enemies.json
"""
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ABYSS = ROOT / "deploy/static/abyss.json"
OUT = ROOT / "blades_lib/src/abyss_kill_multipliers.json"
MAX_LEVEL = 500

# dungeonPool `monsters` token -> prefix of the `* Family Abyss` family names.
TOKENS = {
    "Spriggan": ["Spriggan"],
    "Skeevers": ["Skeever"],
    "Wolves": ["Wolf"],
    "Wolf": ["Wolf"],
    "Bears": ["Bear"],
    "CaveSpiders": ["Spider Cave"],
    "ForestSpiders": ["Spider Forest"],
    "Spider": ["Spider"],
    "Wisp": ["Wisp"],
    "Liches": ["Lich Family"],
    "Lich": ["Lich Family"],
    "OutcastNether": ["Lich Outcast", "Lich Nether"],
    "Wight": ["Wight"],
    "Skeletons": ["Skeleton"],
    "Skeleton": ["Skeleton"],
    "Necromancer": ["Necromancer"],
    "Atronach": ["Flame Atronach", "Frost Atronach", "Storm Atronach", "Mixed Atronach"],
    "Dremora": ["Dremora"],
    "Troll": ["Troll"],
    "Goblin": ["Goblin"],
    "Mercenary": ["Mercenary"],
    "Bandit": ["Bandit"],
    "Warlord": ["Warlord"],
    "Undead": ["Undead Caster"],
    "Wizard": ["Goblin Wizard"],
    "DragonUndead": ["Dragon Undead"],
    "DragonAncientFire": ["Dragon Ancient Family"],
    "DragonAncientFrost": ["Dragon Ancient Frost"],
    "Dragon": ["Dragon Family"],
}


def family_prefixes(monster):
    if monster in TOKENS:
        return TOKENS[monster]
    parts, out, i = re.findall(r"[A-Z][a-z]*", monster), [], 0
    while i < len(parts):
        for width in (3, 2, 1):
            token = "".join(parts[i : i + width])
            if token in TOKENS:
                out += TOKENS[token]
                i += width
                break
        else:
            raise SystemExit(f"unmapped monster token in {monster!r}: {parts[i]!r}")
    return out


def main():
    enemies = json.loads(Path(sys.argv[1]).read_text())
    abyss = json.loads(ABYSS.read_text())
    variants = {v.get("guid", k): v for k, v in enemies["variants"].items()}
    families = {n: f for n, f in enemies["families"].items() if n.endswith("Family Abyss")}

    def band_multiplier(family, level):
        bands = sorted(family["bands"], key=lambda b: b["min_level"])
        band = next((b for b in bands if b["min_level"] <= level <= b["max_level"]), None)
        if band is None:  # outside every band: the nearest end of the range
            band = bands[0] if level < bands[0]["min_level"] else bands[-1]
        return variants[band["variant_guid"]]["stats"]["killScoreMultiplier"]

    dungeons, unmapped = {}, []
    for dungeon_id, dungeon in sorted(abyss["dungeonPool"].items()):
        if dungeon["monsters"] == ["AbyssEntrance"]:
            unmapped.append(dungeon["handle"])  # never a slice: no fixed/random entry
            continue
        fams = sorted(
            {
                name
                for monster in dungeon["monsters"]
                for prefix in family_prefixes(monster)
                for name in families
                if name.startswith(prefix)
            }
        )
        if not fams:
            raise SystemExit(f"no family for {dungeon['handle']} {dungeon['monsters']}")
        steps, last = [], None
        for level in range(1, MAX_LEVEL + 1):
            m = min(band_multiplier(families[f], level) for f in fams)
            if m != last:
                steps.append([level, m])
                last = m
        dungeons[dungeon_id] = {"handle": dungeon["handle"], "families": fams, "steps": steps}

    OUT.write_text(
        json.dumps(
            {
                "_meta": {
                    "description": "Abyss kill-score multiplier per floor dungeon (#312). "
                    "`steps` is [fromEnemyLevel, multiplier] pairs; a step holds until the next.",
                    "rule": "lowest killScoreMultiplier of any candidate variant at that level "
                    "(dungeon monsters -> `* Family Abyss` families -> level band -> variant)",
                    "source": "deploy/static/abyss.json dungeonPool + reference/game-defs/enemies.json "
                    "(APK extraction); regenerate with script/build_abyss_kill_multipliers.py",
                    "notInATable": unmapped,
                },
                "dungeons": dungeons,
            },
            indent=1,
            sort_keys=True,
        )
        + "\n"
    )
    print(f"wrote {OUT.relative_to(ROOT)}: {len(dungeons)} dungeons; skipped {unmapped}")


if __name__ == "__main__":
    main()
