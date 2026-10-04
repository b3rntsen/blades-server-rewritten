#!/usr/bin/env python3
"""Measure the four town merchants' stock per building level from retail captures.

Why this exists
---------------
Report #309: a level-4 Workshop offered a single limestone. Retail's Workshop at
that level stocked 27-29 limestone and 28 lumber, and its quantities climb with
every upgrade (lumber 16 at level 2, about 187 at level 9).

Two defects sat behind it:

  * The Workshop had no measured pools at all, so it rolled from the authored
    fallback pool, where every entry is 1..4 at most.
  * The measured pools the other three shops did have were filed one level low.
    They were joined to the nearest earlier `GET /towns/current`, which misses the
    `/upgrade` and `/complete` responses that also carry the town. A catalog
    opened right after an upgrade finished was therefore filed under the level
    before it, so pool N held level N+1 stock (the Alchemist's level-6 pool
    sold level-7 potions) and `maxItems` / merchant gold drifted the same way
    (the Forge's level-1 gold row was invented and its level-2 row held the
    level-1 catalog).

How this measures
-----------------
Every capture response that carries a town (`GET /towns/current`,
`POST .../buildings`, `.../upgrade`, `.../complete`, `.../styles/{id}`) updates
the character's last known state of each building, in timestamp order. Each
`POST /shops/{id}` that opens a catalog is labelled with the building's level
from that state. Every retail open in the snapshot found its building in the
`NORMAL` state (none mid-upgrade; prod's data adds two UPGRADING opens, which
are skipped), and every catalog template maps to exactly one level, which is
checked below rather than assumed.

Note on the wire: retail raises a building's `level` when the upgrade STARTS
(state `UPGRADING`); `/complete` only clears the state. A town fetched during an
upgrade therefore shows the next level, which is the other way a naive join
mislabels a catalog.

What it writes into `shop_stock.json` (`generation.<typeId>`)
-------------------------------------------------------------
  levelPools.<L>   every bundle retail stocked at level L, with its quantity band
                   and a pick weight fitted so the generator's chance of listing
                   the bundle matches how often retail listed it.
  levels.<L>.maxItems   retail's catalog length at level L (constant per level).
  merchantGold.<L>      only where the old row was unmeasured or carried another
                   level's template; measured rows keep their numbers and are
                   only widened to include every wallet seen here.

Levels with no retail open (Forge 2, Enchanter 0-1, Workshop 0-1, Alchemist 1)
are derived from the neighbouring measured levels and marked so in `_source`.

Reproduce
---------
Report #335 re-measured everything from prod's capture database, which holds
retail traffic to 2026-06-30 (three weeks past the June snapshot #309 used:
663 labelled catalogs instead of about 200; 108 level-9 Alchemist catalogs
instead of 17). Its bodies sit in a second database, so the read runs on the
box and the weight fit (CPU-heavy, about 40 s) runs elsewhere:

  # on the box, read-only
  sudo python3 extract_shop_level_stock.py \\
      --db /var/lib/newblades/db/blades.db \\
      --archive /var/lib/newblades/db/blades-archive.db \\
      --dump-cells /tmp/shop-cells.json
  # locally
  python3 script/extract_shop_level_stock.py --cells shop-cells.json \\
      --stock deploy/static/shop_stock.json --write

`--db` alone still reads a snapshot with bodies inline (table `api_captures`),
e.g. the 2026-06-07 copy. Databases are opened read-only. Only
`https://blades.bgs.services/` rows count: prod's table also holds our own
server's responses from August.
"""

from __future__ import annotations

import argparse
import collections
import datetime
import json
import math
import random
import re
import sqlite3
import statistics
import sys

SHOPS = {
    "26fdb92f-a4df-4928-a97b-dee8699af605": "Forge",
    "82108d94-ebf7-434f-8623-ca66d7504f27": "Enchanter",
    "b6c023e6-3b81-497f-9c2c-f532ecff3bb2": "Workshop",
    "e1dd10fc-8b14-4288-9b23-99b0d58388de": "Alchemist",
}
GOLD = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2"
LEVELS = range(10)
SHOP_OPEN = re.compile(r"/characters/([0-9a-f-]{36})/shops/([0-9a-f-]{36})$")
TOWN_URL = re.compile(r"/characters/([0-9a-f-]{36})/towns/current")


def load_rows(db: str, archive: str | None = None):
    """Retail shop opens and town-bearing responses, oldest first, streamed.

    `archive` is the prod layout: `api_captures` keeps no bodies and they live in
    a second database, `capture_bodies(capture_id PRIMARY KEY, response_body)`.
    Only retail traffic counts: the prod table also holds our own server's
    responses (`http://127.0.0.1:8087/...`), which must never measure retail.
    """
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    if archive:
        con.execute("attach database ? as ar", (f"file:{archive}?mode=ro",))
        body_col = "coalesce(a.response_body, (select b.response_body from ar.capture_bodies b where b.capture_id = a.id))"
    else:
        body_col = "a.response_body"
    yield from con.execute(
        f"""select a.id, a.timestamp, a.url, a.response_status, {body_col} from api_captures a
           where (a.url like '%/shops/%' or a.url like '%/towns/current%')
             and a.url not like '%/social/%' and a.url like 'https://blades.bgs.services/%'
             and a.response_status = 200
           order by a.timestamp, a.id"""
    )
    con.close()


def body(raw):
    if raw is None:
        return None
    if isinstance(raw, bytes):
        raw = raw.decode("utf-8", "replace")
    try:
        return json.loads(raw)
    except ValueError:
        return None


def walk_buildings(node, out):
    if isinstance(node, dict):
        if "typeId" in node and "segmentGroupId" in node and "id" in node:
            out[node["id"]] = node
        for v in node.values():
            walk_buildings(v, out)
    elif isinstance(node, list):
        for v in node:
            walk_buildings(v, out)
    return out


def measure(rows):
    """{(typeId, level): {catalogId: catalog}} plus the template->levels check.

    A catalog can be re-opened after a town fetch this capture never saw (the
    player upgraded on another device, or between captured sessions), so the same
    catalog id can carry two labels. The later open saw the fresher town, so the
    catalog keeps its LAST label: one Forge catalog was opened under a level-3 town
    fetched fifteen hours earlier and again, minutes later, under a fresh level-4
    town, and its template is level 4's in every other open.
    """
    state = {}
    label = {}
    states = collections.Counter()
    for _id, _ts, url, _st, raw in rows:
        path = url.split("?")[0]
        m = SHOP_OPEN.search(path)
        j = body(raw)
        if j is None:
            continue
        if m and isinstance(j.get("catalog"), dict):
            known = state.get((m.group(1), m.group(2)))
            if not known or known["typeId"] not in SHOPS:
                continue
            states[known.get("state")] += 1
            if known.get("state") != "NORMAL":
                continue
            cat = j["catalog"]
            label[cat["id"]] = (known["typeId"], int(known.get("level", 0)), cat)
            continue
        t = TOWN_URL.search(path)
        if t and isinstance(j, dict):
            for bid, b in walk_buildings(j.get("town", j), {}).items():
                state[(t.group(1), bid)] = {k: b.get(k) for k in ("typeId", "level", "state")}
    cells = collections.defaultdict(dict)
    template_levels = collections.defaultdict(set)
    for cid, (type_id, level, cat) in label.items():
        cells[(type_id, level)][cid] = cat
        template_levels[(type_id, cat["templateId"])].add(level)
    bad = {k: v for k, v in template_levels.items() if len(v) != 1}
    if bad:
        sys.exit(f"a catalog template maps to more than one level: {bad}")
    return cells, states


def fit_weights(targets: dict, k: int, iters: int = 60, sims: int = 4000, seed: int = 309):
    """Weights whose weighted draw of k distinct bundles lists each bundle with
    the target probability. Same draw as `shop_gen::generate_catalog`."""
    ids = sorted(targets)
    if len(ids) <= k:
        return {b: 1 for b in ids}
    w = {b: targets[b] for b in ids}
    rng = random.Random(seed)
    for _ in range(iters):
        hits = collections.Counter()
        for _ in range(sims):
            pool = list(ids)
            for _ in range(k):
                total = sum(w[b] for b in pool)
                r = rng.random() * total
                for i, b in enumerate(pool):
                    r -= w[b]
                    if r < 0:
                        break
                hits[pool.pop(i)] += 1
        for b in ids:
            p = max(hits[b], 1) / sims
            w[b] *= (targets[b] / p) ** 0.7
        top = max(w.values())
        w = {b: v / top for b, v in w.items()}
    floor = min(w.values())
    scale = 10 / floor if floor > 0 else 1
    scale = min(scale, 100000 / max(w.values()))
    return {b: max(1, round(v * scale)) for b, v in w.items()}


def measured_pool(cats: dict):
    n = len(cats)
    lengths = collections.Counter(len(c["bundles"]) for c in cats.values())
    k = lengths.most_common(1)[0][0]
    seen = collections.defaultdict(list)
    for c in cats.values():
        for b in c["bundles"]:
            seen[b["id"]].append(int(b["quantity"]))
    # Target listing chance = retail's listing rate. A bundle listed in every
    # catalog cannot be given 100% (its weight would be infinite), so it gets
    # 1 - 1/(2(n+1)) and the rest are rescaled so the chances still add up to k,
    # which is what a k-item draw's listing chances must sum to.
    cap = 1 - 1 / (2 * (n + 1))
    rate = {b: len(q) / n for b, q in seen.items()}
    capped = {b: cap for b, p in rate.items() if p >= cap}
    rest = {b: p for b, p in rate.items() if b not in capped}
    room = k - sum(capped.values())
    scale = room / sum(rest.values()) if rest else 0
    targets = {**capped, **{b: min(cap, p * scale) for b, p in rest.items()}}
    weights = fit_weights(targets, k)
    pool = [
        {
            "bundleId": b,
            "weight": weights[b],
            "minQuantity": min(q),
            "maxQuantity": max(q),
            "_retailListed": f"{len(q)}/{n}",
        }
        for b, q in sorted(seen.items(), key=lambda kv: (-len(kv[1]), kv[0]))
    ]
    wallets = sorted(
        w["balance"] for c in cats.values() for w in c.get("wallet", []) if w["currencyId"] == GOLD
    )
    return pool, k, n, wallets, sorted({c["templateId"] for c in cats.values()})


def growth(pools: dict) -> float:
    """Median per-level quantity growth across bundles stocked at consecutive
    measured levels — how much a stack grows with one upgrade."""
    ratios = []
    for lvl in sorted(pools):
        nxt = pools.get(lvl + 1)
        if not nxt:
            continue
        a = {e["bundleId"]: (e["minQuantity"] + e["maxQuantity"]) / 2 for e in pools[lvl][0]}
        for e in nxt[0]:
            b = a.get(e["bundleId"])
            if b and b >= 2:
                ratios.append(((e["minQuantity"] + e["maxQuantity"]) / 2) / b)
    return statistics.median(ratios) if ratios else 1.0


def scaled(entry: dict, factor: float) -> dict:
    out = dict(entry)
    out["minQuantity"] = max(1, round(entry["minQuantity"] * factor))
    out["maxQuantity"] = max(out["minQuantity"], round(entry["maxQuantity"] * factor))
    out.pop("_retailListed", None)
    return out


def derive(level: int, measured: dict, g: float):
    """Pool + maxItems for a level retail was never seen at."""
    lower = max((l for l in measured if l < level), default=None)
    upper = min((l for l in measured if l > level), default=None)
    if lower is not None and upper is not None:
        lo, hi = measured[lower], measured[upper]
        lo_by = {e["bundleId"]: e for e in lo[0]}
        hi_by = {e["bundleId"]: e for e in hi[0]}
        pool = []
        for b in sorted(set(lo_by) | set(hi_by)):
            if b in lo_by and b in hi_by:
                e = dict(lo_by[b])
                e.pop("_retailListed", None)
                e["minQuantity"] = max(1, round(math.sqrt(lo_by[b]["minQuantity"] * hi_by[b]["minQuantity"])))
                e["maxQuantity"] = max(e["minQuantity"], round(math.sqrt(lo_by[b]["maxQuantity"] * hi_by[b]["maxQuantity"])))
                e["weight"] = max(1, round((lo_by[b]["weight"] + hi_by[b]["weight"]) / 2))
            elif b in lo_by:
                e = scaled(lo_by[b], g ** (level - lower))
            else:
                e = scaled(hi_by[b], g ** (level - upper))
            pool.append(e)
        k = (lo[1] + hi[1]) // 2
        src = f"DERIVED: no retail open at this level; bundles of measured levels {lower} and {upper}, quantities between theirs"
    else:
        near = lower if lower is not None else upper
        pool = [scaled(e, g ** (level - near)) for e in measured[near][0]]
        k = measured[near][1]
        src = f"DERIVED: no retail open at this level; measured level {near}'s bundles, quantities scaled by this shop's per-level growth {g:.2f}^{level - near}"
    return pool, min(k, len(pool)), src


def band(base: float):
    return round(base * 8 / 9), round(base * 10 / 9)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--db", help="capture database (api_captures)")
    src.add_argument("--cells", help="measured cells written earlier by --dump-cells")
    ap.add_argument("--dump-cells", help="write the measured cells here and stop (run the cheap read on the box, fit the weights elsewhere)")
    ap.add_argument("--archive", help="prod layout: the database holding capture_bodies")
    ap.add_argument("--stock", default="deploy/static/shop_stock.json")
    ap.add_argument("--write", action="store_true")
    args = ap.parse_args()

    if args.cells:
        with open(args.cells) as f:
            dumped = json.load(f)
        cells = {(k.split("|")[0], int(k.split("|")[1])): v for k, v in dumped["cells"].items()}
        states = dumped["states"]
    else:
        cells, states = measure(load_rows(args.db, args.archive))
    if args.dump_cells:
        slim = {
            f"{t}|{l}": {cid: {k: c.get(k) for k in ("id", "templateId", "bundles", "wallet")} for cid, c in cats.items()}
            for (t, l), cats in cells.items()
        }
        with open(args.dump_cells, "w") as f:
            json.dump({"cells": slim, "states": dict(states)}, f)
        print(f"wrote {sum(len(v) for v in slim.values())} catalogs in {len(slim)} cells to {args.dump_cells}")
        return
    with open(args.stock) as f:
        stock = json.load(f)

    report = []
    for type_id, name in SHOPS.items():
        gen = stock["generation"][type_id]
        measured = {}
        for lvl in LEVELS:
            cats = cells.get((type_id, lvl))
            if cats:
                measured[lvl] = measured_pool(cats)
        g = growth(measured)
        pools = {}
        for lvl in LEVELS:
            params = gen["levels"][str(lvl)]
            if lvl in measured:
                pool, k, n, wallets, templates = measured[lvl]
                params["maxItems"] = k
                params["_maxItemsSource"] = f"MEASURED: every retail catalog at this level listed {k} bundles ({n} catalogs)"
                params["_retailMeasuredStock"] = f"{n} retail catalogs, template {', '.join(templates)}; script/extract_shop_level_stock.py"
                report.append(f"{name} L{lvl}: {n} catalogs, {k} bundles each, gold {wallets[0]}..{wallets[-1]}")
            else:
                pool, k, src = derive(lvl, measured, g)
                params["maxItems"] = k
                params["_maxItemsSource"] = src
                params["_retailMeasuredStock"] = src
                report.append(f"{name} L{lvl}: no retail sample -> {src}")
            params.pop("_maxItemsWas", None)
            pools[str(lvl)] = pool

        gen["levelPools"] = pools

        # Merchant gold. A row whose template belongs to another level, or that
        # was never measured, takes this measurement; a measured row keeps its
        # numbers but must contain every wallet retail showed at that level.
        gold = gen["merchantGold"]
        tpl_level = {measured[l][4][0]: l for l in measured}
        for lvl in LEVELS:
            row = gold[str(lvl)]
            m = measured.get(lvl)
            owner = tpl_level.get(row.get("capturedCatalogTemplateId"))
            wrong_tpl = owner is not None and owner != lvl
            if m and (row.get("source") != "MEASURED" or wrong_tpl):
                wallets = m[3]
                base = statistics.mean(wallets)
                lo, hi = band(base)
                gold[str(lvl)] = {
                    "bandMax": max(hi, wallets[-1]),
                    "bandMin": min(lo, wallets[0]),
                    "baseGold": round(base),
                    "capturedCatalogTemplateId": m[4][0],
                    "observations": len(wallets),
                    "source": "MEASURED",
                    "_note": f"remeasured for report #309 (was {row.get('source')}"
                    + (f", template {row.get('capturedCatalogTemplateId')} is level {owner}'s" if wrong_tpl else "")
                    + f"); retail wallets {wallets}",
                }
            elif m:
                wallets = m[3]
                if wallets[0] < row["bandMin"] or wallets[-1] > row["bandMax"]:
                    row["_note"] = (
                        f"band widened for report #309 to hold every retail wallet seen at this level "
                        f"(was {row['bandMin']}..{row['bandMax']}); retail wallets {wallets}"
                    )
                    row["bandMin"] = min(row["bandMin"], wallets[0])
                    row["bandMax"] = max(row["bandMax"], wallets[-1])
            elif wrong_tpl or row.get("source") != "MEASURED":
                anchors = [l for l in LEVELS if l != lvl and gold[str(l)].get("source") == "MEASURED"
                           and tpl_level.get(gold[str(l)].get("capturedCatalogTemplateId"), l) == l]
                below = max((l for l in anchors if l < lvl), default=None)
                above = min((l for l in anchors if l > lvl), default=None)
                if below is not None and above is not None:
                    a = gold[str(below)]["baseGold"]
                    b = gold[str(above)]["baseGold"]
                    base = a * (b / a) ** ((lvl - below) / (above - below))
                    lo, hi = band(base)
                    gold[str(lvl)] = {
                        "bandMax": hi,
                        "bandMin": lo,
                        "baseGold": round(base),
                        "observations": 0,
                        "source": "INTERPOLATED",
                        "_note": f"no retail open at this level; geometric between measured levels {below} and {above} (report #309"
                        + (f"; the old row carried level {owner}'s template" if wrong_tpl else "")
                        + ")",
                    }

    stock["_meta"]["_generation"]["levelPoolsSource"] = (
        "script/extract_shop_level_stock.py (reports #309, #335; prod capture DB, retail traffic to 2026-06-30): each retail shop open labelled with its building's level "
        "from the latest town-bearing response (towns/current, upgrade, complete, styles, buildings), a re-opened catalog keeping its last label; "
        f"open states seen {dict(states)}; one template per level verified."
    )
    print("\n".join(report))
    if args.write:
        with open(args.stock, "w") as f:
            json.dump(stock, f, indent=2)
            f.write("\n")


if __name__ == "__main__":
    main()
