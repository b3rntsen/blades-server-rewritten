#!/usr/bin/env python3
"""Derive retail-journey test fixtures from the capture corpus.

The "journey" is the single-player progression loop — create a character, play
quests, spend the XP, build and upgrade the town — and these fixtures are what
retail actually did at each step, so our tests can assert against a measurement
instead of against a guess.

Every value written here is read out of a captured Bethesda response. Nothing is
modelled. Where a number could not be attributed to one request with certainty
the observation is still written, carrying the flags that say so, and the tests
filter on those flags rather than this script silently dropping evidence.

    # local snapshot (bodies inline)
    python3 script/extract_journey_fixtures.py \
        --db ~/blades-prod-backups/20260607-112415/blades-snapshot-20260607-112415.db \
        --out deploy/retail-journey

    # prod (bodies live in the sibling archive db)
    sudo python3 script/extract_journey_fixtures.py \
        --db /var/lib/newblades/db/blades.db \
        --archive /var/lib/newblades/db/blades-archive.db \
        --out /tmp/retail-journey

`--user` defaults to Yumeko, whose 57k requests are the only corpus that covers
the whole journey from level 1 to a level-10 town. Retail hosts only: our own
emulator answers on 127.0.0.1:8087 and treating those as ground truth is
circular — it has produced a wrong conclusion here before.
"""

import argparse
import json
import os
import re
import sqlite3
import sys
from collections import Counter, defaultdict

RETAIL_PREFIX = "https://blades.bgs.services/api/game/v1/public"
GOLD = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2"
SIGIL = "c64bcb53-41f4-41ba-892a-fe2cca423caa"
GEMS = "470c8f58-a8dd-4c07-8c92-843b785e1139"
DEFAULT_USER = "164824943366766592"  # Yumeko

UUID_RE = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")


# ── corpus access ────────────────────────────────────────────────────────────

def open_corpus(db, archive):
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    if archive:
        conn.execute("ATTACH DATABASE ? AS ar", (f"file:{archive}?mode=ro",))
    return conn


def stream(conn, user, archive):
    """Yield (id, ts, path, request, response) for retail game-API calls, in order.

    Bodies were moved out of `api_captures` into `blades-archive.capture_bodies`
    at some point, so both have to be read: on prod every one of this user's rows
    has its body only in the archive, and a query that forgets the join silently
    sees empty responses rather than failing.
    """
    if archive:
        body = ("COALESCE(ar.capture_bodies.request_body, c.request_body), "
                "COALESCE(ar.capture_bodies.response_body, c.response_body)")
        join = "LEFT JOIN ar.capture_bodies ON c.id = ar.capture_bodies.capture_id"
    else:
        body, join = "c.request_body, c.response_body", ""
    sql = (f"SELECT c.id, c.timestamp, c.url, {body} FROM api_captures c {join} "
           f"WHERE c.user_id = ? AND c.url LIKE ? ORDER BY c.id")
    for cid, ts, url, req, resp in conn.execute(sql, (user, RETAIL_PREFIX + "/%")):
        yield cid, ts, url[len(RETAIL_PREFIX):], _json(req), _json(resp)


def _json(blob):
    if not blob:
        return None
    try:
        return json.loads(blob)
    except Exception:
        return None


def balance(wallet, currency):
    if not isinstance(wallet, list):
        return None
    for entry in wallet:
        if isinstance(entry, dict) and entry.get("currencyId") == currency:
            return entry.get("balance")
    return None


def buildings(town):
    """Flatten the nested town JSON to {buildingId: building}.

    A mutation response carries only the districts it touched, so this is a
    partial view — callers merge it into a running picture rather than diffing
    it as if it were the whole town.
    """
    out = {}
    if not isinstance(town, dict):
        return out
    for district in town.get("districts") or []:
        segments = district.get("segments") or {}
        if isinstance(segments, dict):
            segments = list(segments.values())
        for segment in segments:
            if not isinstance(segment, dict):
                continue
            blist = segment.get("buildings")
            if isinstance(blist, dict):
                blist = list(blist.values())
            for b in blist or []:
                if isinstance(b, dict) and b.get("id"):
                    out[b["id"]] = b
    return out


# ── 1. the level-up ladder ───────────────────────────────────────────────────

def extract_levelups(rows):
    """Every `POST /levelup`, with the character's XP either side of it.

    Retail SPENDS the experience: the response's `experience` is what was left
    after the level's threshold was deducted. The "before" therefore has to come
    from the last response that carried the character, which is why this walks
    the whole stream rather than reading the level-up captures alone.
    """
    out = []
    prev = None  # (level, experience, stamina, magicka)
    for cid, ts, path, req, resp in rows:
        if not isinstance(resp, dict):
            continue
        ch = resp.get("character")
        if not (isinstance(ch, dict) and "level" in ch and "experience" in ch):
            continue
        cur = (ch.get("level"), ch.get("experience"),
               ch.get("staminaAttributePoints", 0), ch.get("magickaAttributePoints", 0))
        if path.endswith("/levelup") and prev and cur[0] == prev[0] + 1:
            out.append({
                "captureId": cid,
                "timestamp": ts,
                "attribute": (req or {}).get("attribute"),
                "levelBefore": prev[0],
                "levelAfter": cur[0],
                "experienceBefore": prev[1],
                "experienceAfter": cur[1],
                "experienceSpent": prev[1] - cur[1],
                "staminaBefore": prev[2], "staminaAfter": cur[2],
                "magickaBefore": prev[3], "magickaAfter": cur[3],
                # Retail answers with the currency it just credited, not the
                # whole purse — one entry, and which one depends on the level.
                "walletCurrencyIds": [e.get("currencyId") for e in resp.get("wallet") or []
                                      if isinstance(e, dict)],
                "responseKeys": sorted(resp.keys()),
            })
        prev = cur
    return out


# ── 2. town mutations ────────────────────────────────────────────────────────

TOWN_OPS = {
    "place": re.compile(r"^/characters/[0-9a-f-]+/towns/current/buildings$"),
    "upgrade": re.compile(r"^/characters/[0-9a-f-]+/towns/current/buildings/([0-9a-f-]+)/upgrade$"),
    "complete": re.compile(r"^/characters/[0-9a-f-]+/towns/current/buildings/([0-9a-f-]+)/complete$"),
    "destroy": re.compile(r"^/characters/[0-9a-f-]+/towns/current/buildings/([0-9a-f-]+)/destroy$"),
    "style": re.compile(r"^/characters/[0-9a-f-]+/towns/current/buildings/([0-9a-f-]+)/styles/([0-9a-f-]+)$"),
}


def classify_town_op(path):
    for name, rx in TOWN_OPS.items():
        m = rx.match(path)
        if m:
            return name, (m.group(1) if m.lastindex else None)
    return None, None


def extract_town_ops(rows):
    """Gold and town-XP deltas around every town mutation.

    Attribution is the whole difficulty. Town XP also arrives from quest
    rewards (`reward.townXp`), and a town response only shows the districts it
    touched, so two guards travel with each observation:

      * `pendingQuestTownXp` — reward XP banked since the last town reading. A
        non-zero value means the delta is contaminated and the test must skip it.
      * `buildingKnown` — whether the mutated building was actually in view, so
        its type and level can be trusted.
    """
    out = []
    last_xp = None
    last_level = None
    last_gold = None
    known = {}          # running merge of every building ever seen
    pending_quest_xp = 0

    for cid, ts, path, req, resp in rows:
        if not isinstance(resp, dict):
            continue

        reward = resp.get("reward")
        if isinstance(reward, dict) and reward.get("townXp"):
            pending_quest_xp += reward["townXp"]

        op, building_id = classify_town_op(path)
        town = resp.get("town")
        gold = balance(resp.get("wallet"), GOLD)
        seen = buildings(town) if isinstance(town, dict) else {}

        if op:
            rec = {
                "captureId": cid, "timestamp": ts, "op": op,
                "request": req,
                "pendingQuestTownXp": pending_quest_xp,
            }
            if gold is not None and last_gold is not None:
                rec["goldDelta"] = gold - last_gold
            level_info = town.get("levelInfo") if isinstance(town, dict) else None
            if isinstance(level_info, dict):
                xp = level_info.get("experiencePoints")
                if xp is not None and last_xp is not None:
                    rec["townXpDelta"] = xp - last_xp
                rec["townLevelBefore"] = last_level
                rec["townLevelAfter"] = level_info.get("level")
            # The mutated building: prefer this response's own copy, fall back
            # to the running picture (a `destroy` no longer lists it at all).
            before = known.get(building_id)
            b = seen.get(building_id) or before
            if op == "place":
                new_ids = [i for i in seen if i not in known]
                b = seen[new_ids[0]] if len(new_ids) == 1 else None
                before = None
            rec["buildingKnown"] = b is not None
            if b:
                rec["building"] = {"typeId": b.get("typeId"), "styleId": b.get("styleId"),
                                   "level": b.get("level"), "state": b.get("state"),
                                   "constructionEnd": b.get("constructionEnd")}
            # The style the building wore going IN. A style change's town-XP grant
            # turns out to be a difference between the two, so without this the
            # observation cannot be checked against the table at all.
            if before is not None:
                rec["previousStyleId"] = before.get("styleId")
                rec["previousLevel"] = before.get("level")
            out.append(rec)

        if isinstance(town, dict) and isinstance(town.get("levelInfo"), dict):
            last_xp = town["levelInfo"].get("experiencePoints")
            last_level = town["levelInfo"].get("level")
            pending_quest_xp = 0
        if gold is not None:
            last_gold = gold
        known.update(seen)
    return out


# ── 3. quest completions ─────────────────────────────────────────────────────

COMPLETE_RE = re.compile(r"^/characters/[0-9a-f-]+/quests/([0-9a-f-]+)/complete$")


def extract_quest_completions(rows):
    """`/complete` rewards, keyed by TEMPLATE id.

    An event quest's URL carries a per-character INSTANCE id that resolves to
    nothing; only the `gldQuestId` on its row in a preceding `/quests` response
    names a template. Keying by the URL id is the mistake that once made 78
    reward entries name instances that will never exist again.
    """
    instance_to_template = {}
    rewards = defaultdict(list)
    for cid, ts, path, req, resp in rows:
        if not isinstance(resp, dict):
            continue
        for key in ("quests", "gameEventQuests", "gameEventQuestsInWarning", "jobs"):
            for q in resp.get(key) or []:
                if isinstance(q, dict) and q.get("questId"):
                    instance_to_template[q["questId"]] = q.get("gldQuestId") or q["questId"]
        m = COMPLETE_RE.match(path)
        if not m:
            continue
        reward = resp.get("reward")
        if not isinstance(reward, dict):
            continue
        url_id = m.group(1)
        rewards[instance_to_template.get(url_id, url_id)].append({
            "captureId": cid, "reward": reward,
            "resolvedFromInstance": url_id if instance_to_template.get(url_id, url_id) != url_id else None,
        })

    out = {}
    for template, seen in rewards.items():
        payloads = Counter(json.dumps(s["reward"], sort_keys=True) for s in seen)
        best, n = payloads.most_common(1)[0]
        out[template] = {
            "reward": json.loads(best),
            "observations": len(seen),
            "distinctPayloads": len(payloads),
            "variants": [json.loads(p) for p, _ in payloads.most_common()[1:]],
        }
    return out


# ── 4. objective rewards ─────────────────────────────────────────────────────

OBJECTIVES_RE = re.compile(r"^/characters/[0-9a-f-]+/quests/([0-9a-f-]+)/objectives$")


def extract_objective_rewards(rows):
    """What `/objectives` actually pays, and whether it ever pays items."""
    out = []
    for cid, ts, path, req, resp in rows:
        if not (isinstance(resp, dict) and OBJECTIVES_RE.match(path)):
            continue
        reward = resp.get("reward")
        if not isinstance(reward, dict) or not reward:
            continue
        out.append({"captureId": cid, "request": req, "reward": reward,
                    "responseKeys": sorted(resp.keys())})
    return out



# ── 5. what a style change actually pays ─────────────────────────────────────

def extract_style_prestige(town_ops):
    """The town XP a style change grants, keyed `typeId/level/styleId`.

    Retail pays for a restyle — 124 captured `…/styles/<id>` posts, every one of
    them moving `town.levelInfo.experiencePoints` — and the amount depends only on
    the building type, its level and the style being applied. It is NOT a
    difference from the style being replaced: the same change pays the same amount
    in both directions (aa133662→1d6696b3 pays 82 and the reverse pays 72, every
    time), and the TownHall's two observed restyles pay the full table value with
    nothing subtracted.

    `building_upgrades.json`'s `styleInputs[...].prestigeForLevel` predicts this
    exactly for the TownHall, and is high by a constant for the walls (65) and the
    main gate (115) — the same constant for all three of the wall's styles, so the
    table's SHAPE is right and one of its numbers is not. Rather than guess which,
    the measured values are written out here and used in preference to the table
    where they exist.

    Only observations where the delta is attributable are used: no quest reward
    banked in the same window, and the building actually in view.
    """
    grants = defaultdict(Counter)
    for o in town_ops:
        if o["op"] != "style" or o.get("pendingQuestTownXp"):
            continue
        delta = o.get("townXpDelta")
        b = o.get("building") or {}
        if delta is None or not b.get("typeId") or not b.get("styleId"):
            continue
        grants[f"{b['typeId']}/{b.get('level', 0)}/{b['styleId']}"][delta] += 1

    out = {}
    for key, seen in grants.items():
        value, n = seen.most_common(1)[0]
        out[key] = {"townXp": value, "observations": sum(seen.values())}
        if len(seen) > 1:
            # Never silently pick a winner out of a disagreement.
            out[key]["disagreements"] = dict(seen)
    return out


# ── main ─────────────────────────────────────────────────────────────────────

def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", required=True, help="capture sqlite db (api_captures)")
    ap.add_argument("--archive", help="sibling body archive db, when bodies were moved out")
    ap.add_argument("--user", default=DEFAULT_USER)
    ap.add_argument("--out", required=True, help="output directory")
    args = ap.parse_args()

    conn = open_corpus(args.db, args.archive)
    rows = list(stream(conn, args.user, args.archive))
    if not rows:
        sys.exit("no retail rows for that user — wrong db, wrong user id, or the "
                 "archive db was not attached")
    print(f"read {len(rows)} retail captures", file=sys.stderr)

    os.makedirs(args.out, exist_ok=True)
    meta = {
        "description": "Retail observations behind the progression tests. Measured, "
                       "not modelled: every number is read from a captured Bethesda "
                       "response. Regenerate with script/extract_journey_fixtures.py.",
        "source": os.path.basename(args.db),
        "user": args.user,
        "captures": len(rows),
    }

    town_ops = extract_town_ops(rows)
    for name, data in [
        ("levelups.json", extract_levelups(rows)),
        ("town_ops.json", town_ops),
        ("style_prestige.json", extract_style_prestige(town_ops)),
        ("quest_completions.json", extract_quest_completions(rows)),
        ("objective_rewards.json", extract_objective_rewards(rows)),
    ]:
        payload = {"_meta": dict(meta, count=len(data)), "observations": data}
        with open(os.path.join(args.out, name), "w") as fh:
            json.dump(payload, fh, indent=1, sort_keys=False)
            fh.write("\n")
        print(f"  {name}: {len(data)}", file=sys.stderr)


if __name__ == "__main__":
    main()
