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


def extract_challenges(con):
    """Distinct challenge templates (templateId -> objective + reward) from challenge
    objects in list/progress/complete responses."""
    seen = {}

    def consider(ch):
        tid = ch.get("templateId")
        if tid and tid not in seen and ch.get("objective") and ch.get("reward") is not None:
            seen[tid] = {
                "templateId": tid,
                "objective": ch["objective"],
                "reward": ch["reward"],
            }

    for d in responses(con, "%/challenges%"):
        for ch in (d.get("challengeStatus") or {}).get("active", []):
            consider(ch)
        if isinstance(d.get("challenge"), dict):
            consider(d["challenge"])
    return list(seen.values())


def extract_daily_rewards(con):
    """Distinct daily-reward rotation entries (rewardUid -> {rewardUid, dailyReward})."""
    seen = {}
    for d in responses(con, "%/rewards/current"):
        s = d.get("dailyRewardStatus") or {}
        uid = s.get("rewardUid")
        if uid and uid not in seen and s.get("dailyReward") is not None:
            seen[uid] = {"rewardUid": uid, "dailyReward": s["dailyReward"]}
    return list(seen.values())


def extract_chest_loots(con, cap=40):
    """Representative chest-collect loot bundles (deduped, capped)."""
    seen = {}
    for d in responses(con, "%/chests/%/collect%"):
        reward = d.get("reward")
        if reward is None:
            continue
        key = json.dumps(reward, sort_keys=True)
        if key not in seen:
            seen[key] = reward
        if len(seen) >= cap:
            break
    return list(seen.values())


def extract_game_events(con):
    """Distinct daily/Sigil event templates (by the gameEventInstanceId prefix)."""
    seen = {}
    for d in responses(con, "%/gameevents"):
        for e in d.get("gameEvents", []):
            iid = e.get("gameEventInstanceId", "")
            event_id = iid.split("::")[0] if "::" in iid else iid
            if event_id and event_id not in seen and e.get("questId") and e.get("recurrence"):
                duration = e.get("endTimeSecs", 0) - e.get("startTimeSecs", 0)
                seen[event_id] = {
                    "eventId": event_id,
                    "questId": e["questId"],
                    "recurrence": e["recurrence"],
                    "important": e.get("important", False),
                    "instanceDurationSecs": duration if duration > 0 else 0,
                }
    return list(seen.values())


def extract_salvage_recipes(con):
    """Representative salvage yield per recipeId (first single-item salvage seen).
    The real yield is randomised; we keep one representative bundle per recipe."""
    seen = {}
    q = (
        "SELECT request_body, response_body FROM api_captures "
        "WHERE url LIKE '%/salvages' AND response_status=200"
    )
    for rb, sb in con.execute(q):
        try:
            req = json.loads(astext(rb))
            res = json.loads(astext(sb))
        except Exception:
            continue
        infos = req.get("salvageInfos", [])
        if len(infos) != 1:
            continue
        rid = infos[0].get("recipeId")
        mats = (res.get("reward") or {}).get("stackableItems")
        if rid and mats and rid not in seen:
            seen[rid] = mats
    return seen


def extract_shops(con):
    """Town vendor catalogs. The client renders bundles from its own asset data, so the
    server just serves the bundle-id list + window per shop. Catalogs are keyed by the
    shop TEMPLATE (6 types — smith/general/etc.); `byShop` routes a captured shopId to
    its template, a representative catalog (most bundles seen) is kept per template, and
    `default` is the most common template (fallback for an unseen shopId)."""
    import collections

    by_shop = {}
    best = {}  # templateId -> (nbundles, {bundles, wallet})
    tmpl_count = collections.Counter()
    q = (
        "SELECT response_body FROM api_captures WHERE method='POST' "
        "AND url LIKE '%/shops/%' AND url GLOB '*/shops/????????-????-????-????-????????????' "
        "AND url NOT LIKE '%/social/%' AND response_status=200"
    )
    for (rb,) in con.execute(q):
        try:
            d = json.loads(astext(rb))
        except Exception:
            continue
        shop = d.get("shop") or {}
        cat = d.get("catalog") or {}
        sid, tid = shop.get("id"), cat.get("templateId")
        bundles = cat.get("bundles") or []
        if not (sid and tid):
            continue
        by_shop[sid] = tid
        tmpl_count[tid] += 1
        if tid not in best or len(bundles) > best[tid][0]:
            best[tid] = (len(bundles), {"bundles": bundles, "wallet": cat.get("wallet", [])})
    by_template = {tid: v[1] for tid, v in best.items()}
    default = tmpl_count.most_common(1)[0][0] if tmpl_count else None
    return {"byShop": by_shop, "byTemplate": by_template, "default": default}


def extract_shop_bundles(con):
    """bundleId -> {currencyId, price-per-unit, grant}, from single-bundle buy captures.
    revenue = price paid; the granted item is the inventory backpack delta."""
    bundles = {}
    q = (
        "SELECT request_body, response_body FROM api_captures "
        "WHERE url LIKE '%/characters/%/shops/%/purchase' AND url NOT LIKE '%/social/%' "
        "AND response_status=200"
    )
    for rb, sb in con.execute(q):
        try:
            req = json.loads(astext(rb))
            res = json.loads(astext(sb))
        except Exception:
            continue
        reqb = req.get("bundles") or []
        if len(reqb) != 1:
            continue
        bid = reqb[0].get("id")
        qty = reqb[0].get("quantity") or 1
        if not bid or bid in bundles:
            continue
        rev = res.get("shop", {}).get("revenue") or []
        inv = res.get("inventory", {}).get("backpack", {})
        stacks = inv.get("stackableItems") or []
        items = inv.get("items") or []
        grant = {}
        if stacks:
            grant = {"stackableItems": {stacks[0]["itemTemplateId"]: 1}}
        elif items:
            grant = {"items": [items[0]]}
        bundles[bid] = {
            "currencyId": (rev[0].get("currencyId") if rev else None),
            "price": max(1, (rev[0].get("balance", 0) if rev else 0) // max(1, qty)),
            "grant": grant,
        }
    return bundles


EXTRACTORS = {
    "gifts.json": extract_gifts,
    "announcements.json": extract_announcements,
    "global_shop_overrides.json": extract_global_shop_overrides,
    "iap.json": extract_iap,
    "global_shop_grants.json": extract_global_shop_grants,
    "challenges.json": extract_challenges,
    "daily_rewards.json": extract_daily_rewards,
    "chest_loots.json": extract_chest_loots,
    "game_events.json": extract_game_events,
    "salvage_recipes.json": extract_salvage_recipes,
    "shops.json": extract_shops,
    "shop_bundles.json": extract_shop_bundles,
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
