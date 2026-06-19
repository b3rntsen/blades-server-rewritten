#!/usr/bin/env python3
"""Extract capture-derived static game definitions into deploy/static/*.json.

The retail server held catalogs/templates (gifts, announcements, shop bundles,
recipes, chest loot, the event/sigil-quest library, challenge templates, …) that
our `parsed.json` ships empty. We recover them from captured retail traffic
(`api_captures`) so the server can serve faithful definitions and mutate the
player's wallet/inventory for real.

Source of truth: the retained capture snapshot (prod `/var/tmp/blades-snap.db`,
~46k rows; a filtered copy lives in the dev scratchpad). Re-run after capturing new
sessions to refresh the catalogs.

Usage:
  python3 script/extract_static_data.py --db <snapshot.db> --out deploy/static
"""
import argparse
import json
import sqlite3
from pathlib import Path


def astext(b):
    if b is None:
        return None
    if isinstance(b, bytes):
        try:
            return b.decode("utf-8")
        except Exception:
            return None
    return str(b)


def responses(con, like, status=200):
    """Yield parsed JSON response bodies for URLs matching `like`."""
    q = "SELECT response_body FROM api_captures WHERE url LIKE ? AND response_status=?"
    for (body,) in con.execute(q, (like, status)):
        t = astext(body)
        if not t:
            continue
        try:
            yield json.loads(t)
        except Exception:
            continue


def extract_gifts(con):
    """Distinct global-gift definitions from `GET /globalgifts/{id}` responses."""
    seen = {}
    for d in responses(con, "%/globalgifts/%"):
        ov = (d.get("globalGift") or {}).get("globalGiftOverride")
        if ov and ov.get("globalGiftId") and ov["globalGiftId"] not in seen:
            seen[ov["globalGiftId"]] = ov
    return list(seen.values())


def extract_announcements(con):
    """Union of distinct announcement entries (by id) across all responses."""
    seen = {}
    for d in responses(con, "%/announcements%"):
        for a in d.get("announcements", []):
            if a.get("id") and a["id"] not in seen:
                seen[a["id"]] = a
    return list(seen.values())


def extract_global_shop_overrides(con):
    """Union of all global-shop override offers seen (latest wins), served verbatim."""
    merged = {}
    for d in responses(con, "%/catalogoverrides/globalshop"):
        for offer_id, ov in d.get("globalShopOverrides", {}).items():
            merged[offer_id] = ov
    return {"globalShopOverrides": merged}


def extract_iap(con):
    """Union of IAP fulfillment overrides (priced placeholders), served verbatim."""
    merged = {}
    for d in responses(con, "%/catalogoverrides/iap"):
        for product_id, ov in d.get("fulfillmentOverrides", {}).items():
            merged[product_id] = ov
    return {"fulfillmentOverrides": merged}


def extract_global_shop_grants(con):
    """`globalShopProductId` -> reward, from purchase request/response pairs."""
    grants = {}
    q = (
        "SELECT request_body, response_body FROM api_captures "
        "WHERE url LIKE '%/globalshops/current/purchase' AND response_status=200"
    )
    for rb, sb in con.execute(q):
        try:
            req = json.loads(astext(rb))
            res = json.loads(astext(sb))
        except Exception:
            continue
        pid = req.get("globalShopProductId")
        reward = res.get("reward")
        if pid and reward is not None and pid not in grants:
            grants[pid] = reward
    return grants


EXTRACTORS = {
    "gifts.json": extract_gifts,
    "announcements.json": extract_announcements,
    "global_shop_overrides.json": extract_global_shop_overrides,
    "iap.json": extract_iap,
    "global_shop_grants.json": extract_global_shop_grants,
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", required=True, help="path to the api_captures sqlite DB")
    ap.add_argument("--out", required=True, help="output dir (deploy/static)")
    ap.add_argument("--only", help="comma-separated subset of output files to write")
    args = ap.parse_args()

    con = sqlite3.connect(args.db)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    only = set(args.only.split(",")) if args.only else None

    for fname, fn in EXTRACTORS.items():
        if only and fname not in only:
            continue
        data = fn(con)
        (out / fname).write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")
        print(f"wrote {fname}: {len(data)} entries")


if __name__ == "__main__":
    main()
