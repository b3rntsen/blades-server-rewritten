#!/usr/bin/env python3
"""Edit the app-data tables in the ``common`` bundle that the server cannot change.

Two tables, both client-side, both in ``common``:

* ``InventoryData._playerInventoryLevelList`` — one row per backpack tier:
  ``_level``, ``_cost`` (gems the confirm dialog shows and checks against the
  wallet) and ``_capacity`` (ABSOLUTE extra slots on top of
  ``_initialPlayerInventorySize``; see ``patch_inventory_capacity.py`` for the
  disassembly that settles that). ``--tier L:CAPACITY:COST`` updates the row for
  level L, or appends one copied from the top row when L is new.
* ``GuildExchangeData._cooldownsBetweenRequests._defaultDonationCooldownSeconds``
  — the app's own wait between guild donations (stock 3600). The server sends no
  cooldown and refuses nothing (#462), so this is the only lever.

And one in ``duplicates_sab`` (the mirror copy; the APK's is a stale snapshot):

* ``PlayerStatsData._respecCosts[]._currencyAmount`` — the gem price the respec
  screen shows and checks the wallet against before it sends /respec
  (``AddSufficientResourcesCheck`` in ``ValidateAndRespec``). Retail 1..456 by
  level; ``--respec-cost N`` sets every row to N (the server charges 1, d39).
* ``_maxPlayerLevelAllowedForPurchase`` on every shop product in ``common``
  (``GlobalShopProduct`` in ``_templateList``, IAP ``Product`` in ``_products``).
  The client hides an offer above it, so with levels past 100 (#425) the 422
  Sigil Shop / gem-pack offers capped at exactly 100 vanish at 101.
  ``--shop-max-level OLD:NEW`` raises every cap equal to OLD to NEW; other caps
  (0 = none, 14..55) are left alone.
* ``PlayerStatsData._perLevelData[]`` (+ ``_respecCosts[]``) — the client's
  level table. It ends at 100 (``GetLevelCount() = _perLevelData.Length + 1``)
  and a character above it hangs the client on load (#425). ``--max-level N``
  appends rows 101..N by the server's rule (blades-server-rewritten
  ``LevelUpData::with_max_level``): cost = cost(100) + (cost(100) - cost(99))
  * (n - 100), reward = level n-10's reward, no shop unlocks; and one respec
  row per added level, copied from the last.

Every edit is checked by reading the saved bundle back.

The post-Halloween pack d43-stock-r2 (2026-11-13) was built with exactly:

    ./fetch_pack_inputs.py common duplicates_sab town_alcove_alcove_castle_00
    ./patch_common.py bisect/common.orig bisect/common.d43 \\
        --tier 1:10:5 --tier 2:20:20 --tier 3:30:40 --tier 4:40:80 \\
        --tier 5:50:160 --tier 6:60:320 --tier 7:70:640 --tier 8:80:1280 \\
        --tier 9:90:2560 --tier 10:100:2560 --tier 11:110:2560 --donation-cooldown 1
    ./patch_common.py bisect/duplicates_sab.orig bisect/duplicates_sab.d43 --respec-cost 1
    ./build_pack_v2.py d43-stock-r2 "Post-Halloween stock + app data r2 (d43)" \\
        common=bisect/common.d43 \\
        duplicates_sab=bisect/duplicates_sab.d43#0xd8749 \\
        town_alcove_alcove_castle_00=bisect/town_alcove_alcove_castle_00.orig@alcove_castle_00

The alcove bundle is STOCK bytes, carried only so the pack overlaps hw-webs8 and
therefore replaces it whole on beta (docs/asset-pack-layers.md, "shadowed"); its
new hash is the file's md5, which differs from the stock Hash128 cache key.
duplicates_sab's record has no asset paths and only one anchor, so it is pinned:
0xd8749 holds its served size 2,965,494, sits between dungeonsettings' last path
and emotes_sab's record, and that size occurs nowhere else in the file.
"""
import argparse
import hashlib
import json
import sys

from patch_inventory_capacity import _generator, UnityPy  # noqa: F401  (path setup)


def _find(objs, key):
    for obj in objs:
        if obj.type.name != "MonoBehaviour":
            continue
        try:
            tree = obj.read_typetree()
        except Exception:
            continue
        if isinstance(tree, dict) and key in tree:
            return obj, tree
    raise SystemExit("no MonoBehaviour with %s — wrong bundle?" % key)


def _tiers(tree):
    return {r["_level"]: (r["_capacity"], r["_cost"])
            for r in tree["_playerInventoryLevelList"]}


#: Rewards past 100 repeat the last ten retail levels (101-110 pay 91-100).
REWARD_CYCLE = 10


def extend_levels(tree, max_level):
    """Append client level rows up to ``max_level`` (#425). Returns the new rows.

    ``_perLevelData[i]`` is level ``i + 2`` (level 1 has no row); ``_respecCosts[i]``
    is level ``i + 1``. Refuses a table that is not retail's 2..100 shape.
    """
    rows = tree["_perLevelData"]
    respec = tree["_respecCosts"]
    top = len(rows) + 1
    if top != 100 or len(respec) != 100:
        raise SystemExit("level table is %d rows / respec %d — expected retail's 99/100"
                         % (len(rows), len(respec)))
    base = rows[-1]["_requiredExperience"]
    step = base - rows[-2]["_requiredExperience"]
    added = []
    for n in range(top + 1, max_level + 1):
        row = json.loads(json.dumps(rows[n - REWARD_CYCLE - 2]))
        row["_requiredExperience"] = base + step * (n - top)
        row["_globalShopProductUnlocks"] = []
        rows.append(row)
        respec.append(json.loads(json.dumps(respec[-1])))
        added.append((n, row["_requiredExperience"]))
    return added


SHOP_CAP = "_maxPlayerLevelAllowedForPurchase"


def _shop_caps(node, old=None, new=None):
    """Count shop level caps under ``node`` (and rewrite OLD -> NEW if given)."""
    seen = {}
    stack = [node]
    while stack:
        x = stack.pop()
        if isinstance(x, dict):
            if SHOP_CAP in x:
                v = x[SHOP_CAP]
                if old is not None and v == old:
                    x[SHOP_CAP] = v = new
                seen[v] = seen.get(v, 0) + 1
            stack.extend(x.values())
        elif isinstance(x, list):
            stack.extend(x)
    return seen


def raise_shop_caps(objs, old, new):
    """Rewrite shop caps equal to ``old``; returns the number changed."""
    changed = 0
    for obj in objs:
        if obj.type.name != "MonoBehaviour":
            continue
        try:
            tree = obj.read_typetree()
        except Exception:
            continue
        if not isinstance(tree, dict) or not any(k in tree for k in ("_templateList", "_products")):
            continue
        before = _shop_caps(tree).get(old, 0)
        if before:
            _shop_caps(tree, old, new)
            obj.save_typetree(tree)
            changed += before
    return changed


def apply(env, tiers, donation_cooldown, respec_cost=None, max_level=None, shop_cap=None):
    objs = list(env.objects)
    if shop_cap:
        n = raise_shop_caps(objs, *shop_cap)
        if not n:
            raise SystemExit("no shop offer capped at %d — wrong bundle?" % shop_cap[0])
        print("  shop offers capped at %d raised to %d: %d" % (shop_cap[0], shop_cap[1], n))
    if tiers:
        obj, tree = _find(objs, "_playerInventoryLevelList")
        rows = tree["_playerInventoryLevelList"]
        if not rows:
            raise SystemExit("InventoryData has no rows — refusing to guess the shape")
        for level, cap, cost in tiers:
            row = next((r for r in rows if r["_level"] == level), None)
            if row is None:
                row = dict(max(rows, key=lambda r: r["_level"]))
                row["_level"] = level
                rows.append(row)
            row["_capacity"], row["_cost"] = cap, cost
        rows.sort(key=lambda r: r["_level"])
        obj.save_typetree(tree)
    if donation_cooldown is not None:
        obj, tree = _find(objs, "_cooldownsBetweenRequests")
        tree["_cooldownsBetweenRequests"]["_defaultDonationCooldownSeconds"] = donation_cooldown
        obj.save_typetree(tree)
    if respec_cost is not None or max_level is not None:
        obj, tree = _find(objs, "_respecCosts")
        if respec_cost is not None:
            for row in tree["_respecCosts"]:
                row["_currencyAmount"] = respec_cost
        if max_level is not None:
            extend_levels(tree, max_level)
        obj.save_typetree(tree)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("src")
    ap.add_argument("dst")
    ap.add_argument("--tier", action="append", default=[],
                    help="LEVEL:CAPACITY:COST (capacity absolute, on top of the base 40)")
    ap.add_argument("--donation-cooldown", type=int)
    ap.add_argument("--respec-cost", type=int,
                    help="gem price of a respec at every level (duplicates_sab)")
    ap.add_argument("--max-level", type=int,
                    help="extend the client level table to this level (duplicates_sab, #425)")
    ap.add_argument("--shop-max-level",
                    help="OLD:NEW — raise shop offers capped at exactly OLD to NEW (common, #425)")
    a = ap.parse_args()
    shop_cap = tuple(int(x) for x in a.shop_max_level.split(":")) if a.shop_max_level else None
    tiers = [tuple(int(x) for x in t.split(":")) for t in a.tier]

    env = UnityPy.load(a.src)
    _generator(env, list(env.objects)[0])
    apply(env, tiers, a.donation_cooldown, a.respec_cost, a.max_level, shop_cap)
    data = env.file.save(packer="original")
    with open(a.dst, "wb") as fh:
        fh.write(data)

    # Read back from the bytes we wrote, not the in-memory tree.
    back = UnityPy.load(a.dst)
    objs = list(back.objects)
    _generator(back, objs[0])
    got = _tiers(_find(objs, "_playerInventoryLevelList")[1]) if tiers else {}
    for level, cap, cost in tiers:
        if got.get(level) != (cap, cost):
            raise SystemExit("tier %d reads back as %r" % (level, got.get(level)))
    if a.donation_cooldown is not None:
        cd = _find(objs, "_cooldownsBetweenRequests")[1]["_cooldownsBetweenRequests"]
        if cd["_defaultDonationCooldownSeconds"] != a.donation_cooldown:
            raise SystemExit("donation cooldown reads back as %r" % cd)
        print("  donation cooldown -> %d s" % a.donation_cooldown)
    if a.respec_cost is not None:
        amounts = {r["_currencyAmount"]
                   for r in _find(objs, "_respecCosts")[1]["_respecCosts"]}
        if amounts != {a.respec_cost}:
            raise SystemExit("respec costs read back as %r" % amounts)
        print("  respec cost -> %d gems at every level" % a.respec_cost)
    if shop_cap:
        caps = {}
        for o in objs:
            if o.type.name != "MonoBehaviour":
                continue
            try:
                t = o.read_typetree()
            except Exception:
                continue
            if isinstance(t, dict) and any(k in t for k in ("_templateList", "_products")):
                for v, n in _shop_caps(t).items():
                    caps[v] = caps.get(v, 0) + n
        if caps.get(shop_cap[0]):
            raise SystemExit("%d offers still capped at %d" % (caps[shop_cap[0]], shop_cap[0]))
        print("  shop caps read back: %s" % dict(sorted(caps.items())))
    if a.max_level is not None:
        t = _find(objs, "_perLevelData")[1]
        rows, respec = t["_perLevelData"], t["_respecCosts"]
        if len(rows) + 1 != a.max_level or len(respec) != a.max_level:
            raise SystemExit("level table reads back as %d rows / respec %d"
                             % (len(rows), len(respec)))
        xp = [r["_requiredExperience"] for r in rows]
        if any(b <= x for x, b in zip(xp, xp[1:])):
            raise SystemExit("level costs are not strictly increasing")
        for lv in (100, 101, 110, a.max_level):
            print("  level %3d costs %d" % (lv, xp[lv - 2]))
    for level in sorted(got):
        print("  tier %2d  capacity %3d (total %3d)  cost %d"
              % (level, got[level][0], 40 + got[level][0], got[level][1]))
    print("  %s  %d bytes  md5 %s" % (a.dst, len(data), hashlib.md5(data).hexdigest()))


if __name__ == "__main__":
    sys.exit(main())
