#!/usr/bin/env python3
"""Generate deterministic arena-promotion stackables from captured loot data.

Only fixed, guaranteed, specifically identified stackables are safe to materialize
without running the retail random-loot generator.  The generated lookup includes
every character-level band, including empty ones, so an empty high-level band can
never fall back to a populated lower-level band.

    BLADES_CAPTURE_DIR=/path/to/blades-capture \
        python3 script/gen_arena_promotion_loot.py
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from collections import defaultdict
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "server" / "src" / "arena" / "arena_promotion_loot.rs"


def default_loot_path() -> Path:
    capture_dir = os.environ.get("BLADES_CAPTURE_DIR")
    if capture_dir:
        return Path(capture_dir) / "reference" / "game-defs" / "loot.json"
    sibling = ROOT.parent / "blades-capture" / "reference" / "game-defs" / "loot.json"
    if sibling.exists():
        return sibling
    return Path.home() / "Projects" / "blades-capture" / "reference" / "game-defs" / "loot.json"


def deterministic_stackables(level: dict) -> list[tuple[str, int, str]]:
    totals: dict[str, int] = defaultdict(int)
    names: dict[str, str] = {}
    for tier in level.get("tiers", []):
        for drop in tier.get("drops", []):
            weights = drop.get("count_weights", [])
            if len(weights) != 1 or weights[0].get("weight") != 1:
                continue
            for item in drop.get("items", []):
                uid = item.get("specific_item_uid")
                qty_min = item.get("qty_min")
                qty_max = item.get("qty_max")
                if not uid or not item.get("always_drop") or qty_min != qty_max:
                    continue
                totals[uid] += int(qty_min)
                names[uid] = item.get("specific_item", "unnamed captured item")
    return [(uid, totals[uid], names[uid]) for uid in sorted(totals)]


def render(raw: bytes) -> str:
    source = json.loads(raw)
    live_tables = {
        level["loot_table"]
        for arena in source["matchmaking"]["arenas"]
        for level in arena["levels"]
    }
    tables = source["arena_loot_tables"]
    missing = sorted(live_tables - tables.keys())
    if missing:
        raise ValueError(f"matchmaking references missing arena loot tables: {missing}")

    rows: list[tuple[str, int, list[tuple[str, int, str]]]] = []
    for table_name in sorted(live_tables):
        previous = -1
        for level in tables[table_name]["levels"]:
            minimum_level = int(level["level"])
            if minimum_level <= previous:
                raise ValueError(f"{table_name} character-level bands are not ascending")
            previous = minimum_level
            rows.append((table_name, minimum_level, deterministic_stackables(level)))

    sha = hashlib.sha256(raw).hexdigest()
    lines = [
        "//! Arena-promotion stackables generated from retail's shipped loot tables.",
        "//!",
        "//! `[Class 1 — shipped game data]`. This intentionally includes only fixed,",
        "//! guaranteed entries with a `specific_item_uid`; randomized equipment needs",
        "//! the retail loot generator and is not silently approximated here.",
        "//!",
        "//! Regenerate with `script/gen_arena_promotion_loot.py` and set",
        "//! `BLADES_CAPTURE_DIR` when the capture repository is not a sibling.",
        "",
        "#![allow(dead_code)]",
        "",
        f'pub const SOURCE_LOOT_JSON_SHA256: &str = "{sha}";',
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub struct StackableLoot {",
        "    pub template_uuid: &'static str,",
        "    pub quantity: u64,",
        "}",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub struct PromotionLootBand {",
        "    pub loot_table: &'static str,",
        "    pub min_character_level: u16,",
        "    pub stackables: &'static [StackableLoot],",
        "}",
        "",
        f"pub const PROMOTION_LOOT_BANDS: [PromotionLootBand; {len(rows)}] = [",
    ]
    for table_name, minimum_level, stackables in rows:
        lines.append("    PromotionLootBand {")
        lines.append(f'        loot_table: "{table_name}",')
        lines.append(f"        min_character_level: {minimum_level},")
        if stackables:
            lines.append("        stackables: &[")
            for uid, quantity, name in stackables:
                safe_name = name.replace("\n", " ")
                lines.append(f"            // {safe_name}")
                lines.append(
                    "            StackableLoot { "
                    f'template_uuid: "{uid}", quantity: {quantity} '
                    "},"
                )
            lines.append("        ],")
        else:
            lines.append("        stackables: &[],")
        lines.append("    },")
    lines += [
        "];",
        "",
        "/// Select the highest captured character-level band not above `level`.",
        "/// Empty bands are returned as empty rather than falling back farther.",
        "pub fn stackables_for(loot_table: &str, level: u16) -> &'static [StackableLoot] {",
        "    PROMOTION_LOOT_BANDS",
        "        .iter()",
        "        .filter(|band| band.loot_table == loot_table && band.min_character_level <= level)",
        "        .max_by_key(|band| band.min_character_level)",
        "        .map(|band| band.stackables)",
        "        .unwrap_or(&[])",
        "}",
        "",
        "#[cfg(test)]",
        "mod tests {",
        "    use super::*;",
        "    use uuid::Uuid;",
        "",
        "    #[test]",
        "    fn level_86_arena_one_level_five_awards_transcendent_soul_gems() {",
        '        let loot = stackables_for("LootTable_Arena1_ArenaLevel5", 86);',
        "        assert_eq!(loot.len(), 1);",
        '        assert_eq!(loot[0].template_uuid, "d94bab85-53d5-4c9c-a637-acd94fc66c98");',
        "        assert_eq!(loot[0].quantity, 3);",
        "    }",
        "",
        "    #[test]",
        "    fn level_bands_do_not_bleed_into_each_other() {",
        '        let glorious = stackables_for("LootTable_Arena1_ArenaLevel5", 44);',
        '        assert_eq!(glorious[0].template_uuid, "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb");',
        '        let transcendent = stackables_for("LootTable_Arena1_ArenaLevel5", 45);',
        '        assert_eq!(transcendent[0].template_uuid, "d94bab85-53d5-4c9c-a637-acd94fc66c98");',
        "    }",
        "",
        "    #[test]",
        "    fn every_generated_template_id_is_a_uuid() {",
        "        for band in PROMOTION_LOOT_BANDS {",
        "            for item in band.stackables {",
        "                Uuid::parse_str(item.template_uuid).unwrap();",
        "            }",
        "        }",
        "    }",
        "}",
        "",
    ]
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--loot", type=Path, default=default_loot_path())
    parser.add_argument("--out", type=Path, default=OUT)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    generated = render(args.loot.read_bytes())
    if args.check:
        if not args.out.exists() or args.out.read_text() != generated:
            raise SystemExit(f"generated output differs: {args.out}")
        return
    args.out.write_text(generated)


if __name__ == "__main__":
    main()
