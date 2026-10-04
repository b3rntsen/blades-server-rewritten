#!/usr/bin/env python3
"""Add retail's per-level-band reward ladders to ``deploy/static/event_quests.json``.

Report #333: a level-57 character was paid sub-45 sigils for every event.

Retail did not have ONE reward ladder per event quest. Each instance it minted carried
the ``rewards[]`` + ``finalReward`` of the character's LEVEL BAND: five bands, and
every reward in them scales together — sigils, gold, crafting materials and the final
bonus. ``event_quests.json`` collapsed the five into the most-frequently-observed
payload, so which band anyone was paid was an accident of who happened to be captured
(and ``payableRewards`` mixed bands tier by tier).

Bands. The sigil ladder alone identifies the band. Every captured ladder is one of the
rows of ``SIGIL_BANDS`` below, keyed by the event's first-tier sigil count (its
"family"); zero captured ladders fall outside the table. Level thresholds come from
characters whose level is known when the instance was minted or completed (see
``BAND_MIN_LEVEL``).

Per template, every band observed in the corpus is taken verbatim (``rewards`` and
``finalReward`` from the SAME instance object; the most-observed pair wins). A band
not observed for a template is DERIVED and says so in ``_meta``: its sigils come from
the family row (identical across every template of the family), and everything else
from that template's nearest observed band below it, or above when none is below.

Usage (on the capture host, read-only):

    sudo python3 extract_event_level_bands.py \
        --db /var/lib/newblades/db/blades.db \
        --archive /var/lib/newblades/db/blades-archive.db \
        --event-quests event_quests.json --out event_quests.json
"""

import argparse
import collections
import copy
import gzip
import json
import os
import sqlite3

SIGIL = "c64bcb53-41f4-41ba-892a-fe2cca423caa"
RETAIL_PREFIX = "https://blades.bgs.services/%"

# Lowest character level of each band. Measured (snapshot + prod captures, 2026-10-04):
# band 1 at 10-15, band 2 from 17 (Ruukoto's first band-2 mint) to 25, band 3 from 26
# (Ruukoto: 25 -> band 2, 26 -> band 3, one level apart) to 35, band 4 at 37-43, band 5
# from 48 (Humperdinck) to 100. 16, 36 and 44-47 are unobserved; the thresholds take
# the ten-level spacing the 26 boundary pins.
BAND_MIN_LEVEL = [1, 16, 26, 36, 46]

SIGIL_BANDS = {
    1: [(1, 2, 5, 7, 10), (1, 3, 6, 9, 12), (1, 4, 7, 11, 16), (1, 4, 8, 12, 18), (1, 4, 9, 14, 22)],
    3: [(3, 4, 7, 10, 15), (3, 6, 9, 12, 16), (3, 6, 9, 15, 21), (3, 6, 10, 16, 24), (3, 6, 12, 18, 27)],
    4: [(4, 5, 8, 12, 17), (4, 6, 10, 14, 20), (4, 7, 11, 17, 24), (4, 7, 12, 19, 29), (4, 8, 14, 22, 34)],
    # Family 5's band 1 was never captured.
    5: [None, (5, 8, 12, 18, 26), (5, 8, 14, 21, 32), (5, 9, 15, 24, 37), (5, 9, 16, 27, 43)],
}


def sigils(rewards):
    return tuple((r.get("stackableItems") or {}).get(SIGIL, 0) for r in rewards)


def band_of(rewards):
    """0-based band index of a captured ladder, or None."""
    s = sigils(rewards)
    if not s:
        return None
    row = SIGIL_BANDS.get(s[0], [])
    return row.index(s) if s in row else None


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


def canon(v):
    return json.dumps(v, sort_keys=True)


def observe(db, archive):
    """{gld: {band: Counter(canon((rewards, finalReward)) -> instances)}}."""
    con = sqlite3.connect("file:%s?mode=ro" % db, uri=True)
    join, body = "", "c.response_body"
    if archive and os.path.exists(archive):
        con.execute("ATTACH DATABASE ? AS arch", ("file:%s?mode=ro" % archive,))
        join = "LEFT JOIN arch.capture_bodies b ON b.capture_id = c.id"
        body = "COALESCE(c.response_body, b.response_body)"
    seen = collections.defaultdict(set)  # (gld, pair) -> instance ids
    unclassified = collections.Counter()

    def walk(o):
        if isinstance(o, dict):
            if o.get("type") == "GAME_EVENT" and o.get("rewards") and o.get("gldQuestId"):
                b = band_of(o["rewards"])
                if b is None:
                    unclassified[sigils(o["rewards"])] += 1
                else:
                    pair = canon([o["rewards"], o.get("finalReward")])
                    seen[(o["gldQuestId"], b, pair)].add(o.get("questId"))
            for v in o.values():
                walk(v)
        elif isinstance(o, list):
            for v in o:
                walk(v)

    sql = ("SELECT %s FROM api_captures c %s WHERE c.url LIKE ? AND c.url LIKE ? "
           "AND c.response_status = 200 ORDER BY c.id" % (body, join))
    for suffix in ("%/quests", "%/objectives", "%/complete", "%/accept"):
        for (res,) in con.execute(sql, (RETAIL_PREFIX, suffix)):
            walk(decode(res))
    out = collections.defaultdict(lambda: collections.defaultdict(collections.Counter))
    for (gld, b, pair), inst in seen.items():
        out[gld][b][pair] += len(inst)
    return out, unclassified


def build_bands(tmpl, observed):
    fam = sigils(tmpl["rewards"])[0]
    got = {}
    for b, pairs in observed.items():
        pair, n = pairs.most_common(1)[0]
        rewards, final = json.loads(pair)
        got[b] = {"rewards": rewards, "finalReward": final,
                  "_meta": {"instancesObserved": n,
                            "alternatives": sum(pairs.values()) - n}}
    bands = []
    for b, min_level in enumerate(BAND_MIN_LEVEL):
        if b in got:
            band = got[b]
        else:
            below = [k for k in sorted(got) if k < b]
            above = [k for k in sorted(got) if k > b]
            src = below[-1] if below else above[0]
            band = copy.deepcopy(got[src])
            # A band no template of the family was captured in takes the next
            # known band's sigils: never more than the lowest band retail showed.
            row = next(k for k in range(b, len(BAND_MIN_LEVEL)) if SIGIL_BANDS[fam][k])
            note = "sigils from the family-%d band-%d ladder" % (fam, row + 1)
            if row != b:
                note = "family %d's band %d was never captured; %s" % (fam, b + 1, note)
            for tier, n in zip(band["rewards"], SIGIL_BANDS[fam][row]):
                tier.setdefault("stackableItems", {})[SIGIL] = n
            band["_meta"] = {"derived": True,
                             "note": "%s; everything else from band %d" % (note, src + 1)}
        bands.append({"minLevel": min_level, **band})
    return bands


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", required=True)
    ap.add_argument("--archive")
    ap.add_argument("--event-quests", required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    data = json.load(open(a.event_quests))
    observed, unclassified = observe(a.db, a.archive)
    for gld, tmpl in data["templates"].items():
        obs = observed.get(gld)
        if not obs:
            # Authored (#189) templates: their own rewards[] is the one band we have.
            b = band_of(tmpl["rewards"])
            if b is None:
                continue
            obs = {b: collections.Counter({canon([tmpl["rewards"], tmpl.get("finalReward")]): 0})}
        tmpl["levelBands"] = build_bands(tmpl, obs)
    data["_meta"]["levelBands"] = (
        "Report #333. Retail paid each event instance the rewards[] + finalReward of the "
        "character's level band; levelBands holds all five (minLevel %s). A band carries "
        "_meta.instancesObserved when captured verbatim, _meta.derived when it is not. "
        "Generated by script/extract_event_level_bands.py; %d captured ladders matched no "
        "band." % (BAND_MIN_LEVEL, sum(unclassified.values())))
    with open(a.out, "w") as f:
        json.dump(data, f, indent=1)
        f.write("\n")
    print("unclassified", dict(unclassified))


if __name__ == "__main__":
    main()
