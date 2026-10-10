#!/usr/bin/env python3
"""Measure which crafting material retail's event quests paid at each character level.

Reports #362 and #367: event tiers paid the same material at every level inside a
sigil band (Ebony ingots at level 26, Common soul gems where retail had another).

Retail minted each event instance with the material of the character's OWN level,
not of its band. ``blades_lib/src/event_material_ladders.json`` holds the resulting
ladders; this script produces the evidence for them:

* every retail ``GAME_EVENT`` instance in ``/quests``, ``/objectives``, ``/complete``
  and ``/accept`` responses, keyed by ``questId``;
* the character's level at the instance's first sighting, from any character object
  (``id`` + ``level`` + ``experience``) in the same capture set;
* for each laddered item in ``rewards[]`` and ``finalReward``, one observation
  ``[gldQuestId, level, "rewards" | "final", itemId]``.

An instance whose sigil ladder belongs to a different level band than its sighting
level was minted before a level-up and is skipped as a stale level.

It then checks every observation against the ladders (with the same family choice the
server makes) and writes the observations to
``blades_lib/src/event_material_observations.json``, which the server's tests assert.

Usage (on the capture host, read-only; repeat --db for several capture databases):

    sudo -u newblades python3 extract_event_material_ladders.py \
        --db /var/lib/newblades/db/blades.db \
        --archive /var/lib/newblades/db/blades-archive.db \
        --ladders blades_lib/src/event_material_ladders.json \
        --event-quests deploy/static/event_quests.json \
        --out blades_lib/src/event_material_observations.json
"""

import argparse
import bisect
import collections
import gzip
import json
import os
import re
import sqlite3

SIGIL = "c64bcb53-41f4-41ba-892a-fe2cca423caa"
RETAIL_LO, RETAIL_HI = "https://blades.bgs.services/", "https://blades.bgs.services0"
SHUTDOWN = "2026-06-30"
CHAR_IN_URL = re.compile(r"/characters?/([0-9a-f-]{36})")
BAND_MIN_LEVEL = [1, 16, 26, 36, 46]
# Same table as extract_event_level_bands.py: first-tier sigils -> band ladders.
SIGIL_BANDS = {
    1: [(1, 2, 5, 7, 10), (1, 3, 6, 9, 12), (1, 4, 7, 11, 16), (1, 4, 8, 12, 18), (1, 4, 9, 14, 22)],
    3: [(3, 4, 7, 10, 15), (3, 6, 9, 12, 16), (3, 6, 9, 15, 21), (3, 6, 10, 16, 24), (3, 6, 12, 18, 27)],
    4: [(4, 5, 8, 12, 17), (4, 6, 10, 14, 20), (4, 7, 11, 17, 24), (4, 7, 12, 19, 29), (4, 8, 14, 22, 34)],
    5: [None, (5, 8, 12, 18, 26), (5, 8, 14, 21, 32), (5, 9, 15, 24, 37), (5, 9, 16, 27, 43)],
}
URL_GLOBS = ("*/quests", "*/objectives", "*/complete", "*/accept", "*/levelup", "*/data",
             "*/gameevents", "*/public/characters",
             "*/public/characters/????????-????-????-????-????????????")


def decode(blob):
    if blob is None:
        return None
    if isinstance(blob, str):
        blob = blob.encode()
    if blob[:2] == b"\x1f\x8b":
        try:
            blob = gzip.decompress(blob)
        except Exception:
            return None
    try:
        return json.loads(blob)
    except Exception:
        return None


def mine(db, archive):
    """(levels {char: [(ts, level)]}, instances {questId: {...}}) from one capture DB."""
    con = sqlite3.connect("file:%s?mode=ro" % db, uri=True)
    has_arch = bool(archive) and os.path.exists(archive)
    if has_arch:
        con.execute("ATTACH DATABASE ? AS arch", ("file:%s?mode=ro" % archive,))
    where = " OR ".join("url GLOB ?" for _ in URL_GLOBS)
    rows = con.execute(
        "SELECT id, timestamp, url, response_body FROM api_captures WHERE url >= ? AND url < ? "
        "AND response_status = 200 AND timestamp < ? AND (%s)" % where,
        (RETAIL_LO, RETAIL_HI, SHUTDOWN) + URL_GLOBS).fetchall()
    levels, inst = collections.defaultdict(set), {}
    for cid_, ts, url, body in rows:
        if body is None and has_arch:
            got = con.execute("SELECT response_body FROM arch.capture_bodies WHERE capture_id = ?",
                              (cid_,)).fetchone()
            body = got[0] if got else None
        doc = decode(body)
        if doc is None:
            continue
        m = CHAR_IN_URL.search(url)
        url_char = m.group(1) if m else None
        stack = [doc]
        while stack:
            o = stack.pop()
            if isinstance(o, dict):
                if (isinstance(o.get("level"), int) and "experience" in o
                        and isinstance(o.get("id"), str) and len(o["id"]) == 36):
                    levels[o["id"]].add((ts, o["level"]))
                if o.get("type") == "GAME_EVENT" and o.get("gldQuestId") and o.get("rewards"):
                    e = inst.setdefault(o.get("questId"), {
                        "gld": o["gldQuestId"], "char": url_char, "first": ts,
                        "rewards": o["rewards"], "final": o.get("finalReward")})
                    e["first"] = min(e["first"], ts)
                    e["char"] = e["char"] or url_char
                stack.extend(o.values())
            elif isinstance(o, list):
                stack.extend(o)
    return levels, inst


def level_at(levels, char, ts):
    seq = levels.get(char)
    if not seq:
        return None
    i = bisect.bisect_right(seq, (ts, 10 ** 6))
    return seq[i - 1][1] if i else seq[0][1]


def band_of(rewards):
    s = tuple((r.get("stackableItems") or {}).get(SIGIL, 0) for r in rewards)
    row = SIGIL_BANDS.get(s[0] if s else None, [])
    return row.index(s) if s in row else None


def band_at(level):
    return sum(1 for m in BAND_MIN_LEVEL[1:] if level >= m)


class Ladders:
    """The server's choice, mirrored: `blades_lib::features::event_materials`."""

    def __init__(self, path):
        self.ladders = [[(s["minLevel"], s["item"]) for s in l["steps"]]
                        for l in json.load(open(path))["ladders"]]

    def pick(self, item, slot):
        cands = [l for l in self.ladders if any(x == item for _, x in l)]
        if len(cands) <= 1:
            return cands[0] if cands else None

        def unique(l):
            others = {x for o in cands if o is not l for _, x in o}
            return sum(1 for x in slot if x not in others and any(x == y for _, y in l))
        return max(cands, key=unique)  # first wins a tie, as in Rust

    @staticmethod
    def at(ladder, level):
        out = ladder[0][1]
        for m, x in ladder:
            if m <= level:
                out = x
        return out


def slot_items(tmpl, slot):
    s = set()
    for b in tmpl.get("levelBands") or [{"rewards": tmpl["rewards"],
                                          "finalReward": tmpl.get("finalReward")}]:
        if slot == "rewards":
            for r in b["rewards"]:
                s |= set(r.get("stackableItems") or {})
        else:
            s |= set((b.get("finalReward") or {}).get("stackableItems") or {})
    return s


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", action="append", required=True)
    ap.add_argument("--archive")
    ap.add_argument("--ladders", required=True)
    ap.add_argument("--event-quests", required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    levels, inst = collections.defaultdict(set), {}
    for db in a.db:
        lv, ins = mine(db, a.archive)
        for c, v in lv.items():
            levels[c] |= v
        for q, e in ins.items():
            if q not in inst or e["first"] < inst[q]["first"]:
                inst[q] = e
    levels = {c: sorted(v) for c, v in levels.items()}

    lad = Ladders(a.ladders)
    templates = json.load(open(a.event_quests))["templates"]
    obs, stale, wrong, chars = set(), 0, [], set()
    seen_at = collections.defaultdict(list)
    for e in inst.values():
        tmpl = templates.get(e["gld"])
        level = level_at(levels, e["char"], e["first"])
        if tmpl is None or level is None:
            continue
        b = band_of(e["rewards"])
        if b is not None and b != band_at(level):
            stale += 1
            continue
        for slot, grants in (("rewards", e["rewards"]), ("final", [e["final"] or {}])):
            items = {k for g in grants for k in (g.get("stackableItems") or {})}
            for item in items:
                ladder = lad.pick(item, slot_items(tmpl, slot))
                if ladder is None:
                    continue
                obs.add((e["gld"], level, slot, item))
                chars.add(e["char"])
                seen_at[item].append(level)
                if Ladders.at(ladder, level) != item:
                    wrong.append((e["gld"], level, slot, item, Ladders.at(ladder, level)))

    for item, lv in sorted(seen_at.items(), key=lambda kv: min(kv[1])):
        print("%s levels %d-%d (%d)" % (item, min(lv), max(lv), len(lv)))
    print("observations %d, contradicted %d, stale-level instances %d, templates %d, characters %d"
          % (len(obs), len(wrong), stale, len({o[0] for o in obs}), len(chars)))
    for w in wrong:
        print("CONTRADICTED", w)
    meta = {"description": "Retail event-quest materials by the claimant's level (reports #362, "
                           "#367): [gldQuestId, level at first sighting, slot, itemId].",
            "generator": "script/extract_event_material_ladders.py"}
    with open(a.out, "w") as f:
        f.write('{\n "_meta": %s,\n "observations": [\n' % json.dumps(meta))
        f.write(",\n".join("  " + json.dumps(list(o)) for o in sorted(obs)))
        f.write("\n ]\n}\n")


if __name__ == "__main__":
    main()
