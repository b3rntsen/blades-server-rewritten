#!/usr/bin/env python3
"""Generate `block_temper.rs` and `armor_temper.rs` (server/src/arena/combat) from `tempering.json`.

The tempered BLOCK value of every weapon and shield template, and the tempered
ARMOR RATING of every armour template, per tempering level.
Same convention as `gamedata.rs` / `pvp_tuning.rs`: pure const tables, no build.rs,
no runtime serde, the source hash embedded.

    python3 script/gen_block_temper_rs.py --tempering <blades-capture>/reference/game-defs/tempering.json

`tempering.json` is extracted by blades-capture
`reference/game-defs/extract/x_tempering.py` (ItemTemplate._temperProperties, per
item). Only the `protection` column is used: for a weapon or a shield it is the
block value `GetDisplayedBlockRatingForItem@0x1c56504` reads for a tempered item
(`GetTemperPropertiesForLevel(level) +0x18`); for armour it is the Armor Rating
`GetDisplayedProtectionRatingForItem@0x1c56260` reads from the same offset.

Row indexing: row `i` is tempering level `i + 1`. The identity that pins it is the
DAMAGE column: across the weapon templates, `levels[L-1].damage == base_damage +
QUALITY_BONUS[L] x weight` for every L in 1..=10 on 325 of 332 templates, and the
direct reading `levels[L]` holds on none. Row 0 is therefore tempering level 1, and
an untempered item (level 0) uses the template's own `blockBase`. (tempering.json's
own `_meta` note says levels[0] is the untempered baseline; the data contradicts it:
Daedric Shield levels[0] is 303 against blockBase 300.)
"""
import argparse
import hashlib
import json
import os
import re
import struct

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GAMEDATA = os.path.join(ROOT, "server", "src", "arena", "combat", "gamedata.rs")
OUT = os.path.join(ROOT, "server", "src", "arena", "combat", "block_temper.rs")
ARMOR_OUT = os.path.join(ROOT, "server", "src", "arena", "combat", "armor_temper.rs")


def template_uuids(src, table):
    m = re.search(r"pub const %s: \[\w+; \d+\] = \[(.*?)\n\];" % table, src, re.S)
    assert m, table
    return re.findall(r'uuid: "([0-9a-f-]{36})"', m.group(1))


def _f32(x):
    return struct.unpack("<f", struct.pack("<f", float(x)))[0]


def f32_lit(x):
    """Shortest decimal that round-trips to the same f32 the client stores."""
    target = _f32(x)
    for decimals in range(0, 10):
        s = "%.*f" % (decimals, target)
        if _f32(float(s)) == target:
            break
    if "." not in s:
        s += ".0"
    return s


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tempering", required=True, help="blades-capture reference/game-defs/tempering.json")
    args = ap.parse_args()

    raw = open(args.tempering, "rb").read()
    sha = hashlib.sha256(raw).hexdigest()
    items = json.loads(raw)["items"]
    src = open(GAMEDATA).read()

    write_table(
        src, items, sha, ("WEAPONS", "SHIELDS"), OUT, BLOCK_DOC,
        "BLOCK_TEMPER", "block value", "tempered_block_value",
    )
    write_table(
        src, items, sha, ("ARMORS",), ARMOR_OUT, ARMOR_DOC,
        "ARMOR_TEMPER", "Armor Rating", "tempered_armor_value", skip_zero=True,
    )


BLOCK_DOC = [
    "**Tempered block values** — the `protection` column of every weapon and",
    "shield template's `_temperProperties`, per tempering level.",
    "",
    "`GetDisplayedBlockRatingForItem@0x1c56504` reads the temper-level block value",
    "(`GetTemperPropertiesForLevel(level) +0x18`) for a tempered item and",
    "`blockBase` for an untempered one; `get_EquippedBlockRating@0x1c54420` then",
    "ceils it. Row `i` is tempering level `i + 1` (see the generator for the",
    "identity that pins the indexing).",
]

ARMOR_DOC = [
    "**Tempered Armor Ratings** — the `protection` column of every armour",
    "template's `_temperProperties`, per tempering level.",
    "",
    "`Actor$$GetDisplayedProtectionRatingForItem@0x1c56260` reads the temper-level",
    "value (`GetTemperPropertiesForLevel(level) +0x18`) for a tempered piece and the",
    "template's `armorRating` (+0x110) for an untempered one, then `CeilToInt`s it;",
    "`GetEquippedProtectionRating@0x1c55da4` sums those integers. A material",
    "property's `GetRawXValue@0x24d0ae4` (`_propertyType == 2`, armour) multiplies by",
    "the same temper-level value, un-ceiled. Row `i` is tempering level `i + 1`.",
]


def write_table(src, items, sha, tables, out, doc, const, what, fn, skip_zero=False):
    rows = []
    for table in tables:
        for u in template_uuids(src, table):
            t = items.get(u)
            if not t:
                continue
            levels = t["levels"]
            assert len(levels) == 10, (u, len(levels))
            if skip_zero and not any(lv["protection"] for lv in levels):
                # Untemperable (Thalmor clothes ship ten zero rows): keep the
                # template's own value rather than a zero.
                continue
            rows.append((u, t["editor_name"], [lv["protection"] for lv in levels]))
    rows.sort(key=lambda r: r[0])
    assert len({r[0] for r in rows}) == len(rows)

    L = []
    w = L.append
    for line in doc:
        w(("//! " + line).rstrip())
    w("//!")
    w("//! ```text")
    w("//! python3 script/gen_block_temper_rs.py --tempering <blades-capture>/reference/game-defs/tempering.json")
    w("//! ```")
    w("")
    w("// ---------------------------- generated below, do not hand-edit ----------------------------")
    w("")
    w("/// sha256 of `reference/game-defs/tempering.json`.")
    w('pub const SOURCE_SHA256: &str = "%s";' % sha)
    w("")
    w("/// `(template uuid, %s at tempering levels 1..=10)`, sorted by uuid." % what)
    w("pub static %s: [(&str, [f32; 10]); %d] = [" % (const, len(rows)))
    for u, name, prot in rows:
        w('    // %s' % name)
        w('    ("%s", [%s]),' % (u, ", ".join(f32_lit(p) for p in prot)))
    w("];")
    w("")
    w("/// The %s of template `uuid` at `tempering_level` (1..=10; higher" % what)
    w("/// levels clamp to 10), or `None` for level 0 or a template with no temper table.")
    w("pub fn %s(uuid: &str, tempering_level: u64) -> Option<f32> {" % fn)
    w("    if tempering_level == 0 {")
    w("        return None;")
    w("    }")
    w("    let i = %s.binary_search_by(|(u, _)| (*u).cmp(uuid)).ok()?;" % const)
    w("    let row = (tempering_level.min(10) - 1) as usize;")
    w("    Some(%s[i].1[row])" % const)
    w("}")
    open(out, "w").write("\n".join(L) + "\n")
    print("wrote %s: %d templates, source sha256 %s" % (out, len(rows), sha))


if __name__ == "__main__":
    main()
