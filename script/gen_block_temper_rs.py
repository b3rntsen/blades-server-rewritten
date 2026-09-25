#!/usr/bin/env python3
"""Generate `server/src/arena/combat/block_temper.rs` from `tempering.json`.

The tempered BLOCK value of every weapon and shield template, per tempering level.
Same convention as `gamedata.rs` / `pvp_tuning.rs`: pure const tables, no build.rs,
no runtime serde, the source hash embedded.

    python3 script/gen_block_temper_rs.py [--tempering <path to tempering.json>]

`tempering.json` is extracted by blades-capture
`reference/game-defs/extract/x_tempering.py` (ItemTemplate._temperProperties, per
item). Only the `protection` column is used: for a weapon or a shield it is the
block value `GetDisplayedBlockRatingForItem@0x1c56504` reads for a tempered item
(`GetTemperPropertiesForLevel(level) +0x18`).

Row indexing: row `i` is tempering level `i + 1`. The identity that pins it is the
DAMAGE column: across the weapon templates, `levels[L-1].damage == base_damage +
QUALITY_BONUS[L] x weight` for every L in 1..=10 on 325 of 332 templates, and the
direct reading `levels[L]` holds on none. Row 0 is therefore tempering level 1, and
an untempered item (level 0) uses the template's own `blockBase`.
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
DEFAULT_TEMPERING = os.path.expanduser(
    "~/Projects/blades-capture/reference/game-defs/tempering.json"
)


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
    ap.add_argument("--tempering", default=DEFAULT_TEMPERING)
    args = ap.parse_args()

    raw = open(args.tempering, "rb").read()
    sha = hashlib.sha256(raw).hexdigest()
    items = json.loads(raw)["items"]
    src = open(GAMEDATA).read()

    rows = []
    for table in ("WEAPONS", "SHIELDS"):
        for u in template_uuids(src, table):
            t = items.get(u)
            if not t:
                continue
            levels = t["levels"]
            assert len(levels) == 10, (u, len(levels))
            rows.append((u, t["editor_name"], [lv["protection"] for lv in levels]))
    rows.sort(key=lambda r: r[0])
    assert len({r[0] for r in rows}) == len(rows)

    L = []
    w = L.append
    w("//! **Tempered block values** — the `protection` column of every weapon and")
    w("//! shield template's `_temperProperties`, per tempering level.")
    w("//!")
    w("//! `GetDisplayedBlockRatingForItem@0x1c56504` reads the temper-level block value")
    w("//! (`GetTemperPropertiesForLevel(level) +0x18`) for a tempered item and")
    w("//! `blockBase` for an untempered one; `get_EquippedBlockRating@0x1c54420` then")
    w("//! ceils it. Row `i` is tempering level `i + 1` (see the generator for the")
    w("//! identity that pins the indexing).")
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
    w("/// `(template uuid, block value at tempering levels 1..=10)`, sorted by uuid.")
    w("pub static BLOCK_TEMPER: [(&str, [f32; 10]); %d] = [" % len(rows))
    for u, name, prot in rows:
        w('    // %s' % name)
        w('    ("%s", [%s]),' % (u, ", ".join(f32_lit(p) for p in prot)))
    w("];")
    w("")
    w("/// The block value of template `uuid` at `tempering_level` (1..=10; higher")
    w("/// levels clamp to 10), or `None` for level 0 or a template with no temper table.")
    w("pub fn tempered_block_value(uuid: &str, tempering_level: u64) -> Option<f32> {")
    w("    if tempering_level == 0 {")
    w("        return None;")
    w("    }")
    w("    let i = BLOCK_TEMPER.binary_search_by(|(u, _)| (*u).cmp(uuid)).ok()?;")
    w("    let row = (tempering_level.min(10) - 1) as usize;")
    w("    Some(BLOCK_TEMPER[i].1[row])")
    w("}")
    open(OUT, "w").write("\n".join(L) + "\n")
    print("wrote %s: %d templates, source sha256 %s" % (OUT, len(rows), sha))


if __name__ == "__main__":
    main()
