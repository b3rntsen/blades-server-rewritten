#!/usr/bin/env python3
"""Mine retail's Abyss rewards -> blades_lib/src/abyss_future_rewards.json (#172, #294).

Two reward streams, both read from retail `/abysses` traffic in the capture DB:

  * the score gauge (`abyssFutureRewards` on /start, /current, /update): ten rungs,
    35 .. 650. Rungs 50 and 360 are chests (fixed tier, the character's own level).
    The rest draw a stackable (or gear at 260) from a pool.
  * the `/end` package: one stackable -- a crafting material, or a stack of soul
    gems -- on top of the per-floor gold/XP, when the run scored anything worth
    paying for.

WHAT CHANGED SINCE THE FIRST MINER (#294)
-----------------------------------------
The first version deduplicated on (rung, contents, level) and then threw the level
away, so a level-100 character drew from a pool that was mostly level-3..40 results
(Garlic x6, Brass Ingot x4 at rung 70). Every observation now keeps the CHARACTER
LEVEL it was made at, and the unit of observation is one RUN (seed) per rung: the
client re-reads /abysses/current constantly and is re-advertised the same rung, so
that is one draw, not hundreds.

Run on the capture box, read-only:

    scp scripts/mine-abyss-rewards.py newblades.dethele.com:/tmp/
    ssh newblades.dethele.com sudo -n timeout 110 python3 /tmp/mine-abyss-rewards.py \
        --out /tmp/abyss_future_rewards.json
"""
import argparse, collections, json, re, sqlite3, sys

DB = "file:/var/lib/newblades/db/blades.db?mode=ro"
ARCHIVE = "file:/var/lib/newblades/db/blades-archive.db?mode=ro"
RETAIL = "https://blades.bgs.services/"
LADDER = [35, 50, 70, 95, 135, 190, 260, 360, 490, 650]
CHAR_RE = re.compile(r"characters/([0-9a-f-]{36})/abysses(.*)$")


def js(blob):
    if blob is None:
        return None
    if isinstance(blob, bytes):
        try:
            blob = blob.decode()
        except UnicodeDecodeError:
            return None
    try:
        return json.loads(blob)
    except ValueError:
        return None


def strip_ids(o):
    """Drop per-drop instance ids; keep definition ids."""
    if isinstance(o, dict):
        return {k: strip_ids(v) for k, v in o.items() if k != "id"}
    if isinstance(o, list):
        return [strip_ids(v) for v in o]
    return o


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    con = sqlite3.connect(DB, uri=True, timeout=30)
    con.execute(f"ATTACH DATABASE '{ARCHIVE}' AS arc")

    rows = con.execute(
        "SELECT c.id, c.url, COALESCE(b.request_body, c.request_body),"
        " COALESCE(b.response_body, c.response_body)"
        " FROM api_captures c LEFT JOIN arc.capture_bodies b ON b.capture_id = c.id"
        " WHERE c.url LIKE ? AND c.url LIKE '%/abysses%' AND c.response_status = 200"
        " ORDER BY c.id LIMIT 20000",
        (RETAIL + "%",),
    ).fetchall()

    runs = {}          # (char, seed) -> run
    current = {}       # char -> (char, seed)
    for cid, url, rq, rs in rows:
        m = CHAR_RE.search(url or "")
        rs = js(rs)
        if not m or not isinstance(rs, dict):
            continue
        char, path = m.group(1), m.group(2).split("?")[0]
        ab = rs.get("abyss")
        if isinstance(ab, dict) and ab.get("seed") is not None:
            key = (char, ab["seed"])
            current[char] = key
            sl = ab.get("slices") or []
            runs.setdefault(key, {
                "char": char, "seed": ab["seed"], "ipl": ab.get("initialPlayerLevel"),
                "startFloor": sl[0].get("floorIndex") if sl else None,
                "levels": [], "rungs": {}, "end": None,
            })
        key = current.get(char)
        if key is None:
            continue
        run = runs[key]
        ch = rs.get("character")
        if isinstance(ch, dict) and ch.get("level"):
            run["levels"].append(ch["level"])
        fr = rs.get("abyssFutureRewards")
        if fr is None and isinstance(ab, dict):
            fr = ab.get("abyssFutureRewards")
        for f in fr or []:
            if isinstance(f, dict) and f.get("score") in LADDER:
                run["rungs"][f["score"]] = strip_ids(f.get("reward") or {})
        if path == "/current/end" and isinstance(rs.get("reward"), dict):
            run["end"] = strip_ids(rs["reward"])

    # A run seen only through /start or /current carries no `character`; a chest rung
    # is cut to the character's level (72/72), so it names the level too.
    for run in runs.values():
        if not run["levels"]:
            for rw in run["rungs"].values():
                for c in rw.get("chests") or []:
                    run["levels"].append(c.get("level"))
    unknown = [r for r in runs.values() if not r["levels"]]
    for run in unknown:
        for (rs,) in con.execute(
            "SELECT COALESCE(b.response_body, c.response_body) FROM api_captures c"
            " LEFT JOIN arc.capture_bodies b ON b.capture_id = c.id"
            " WHERE c.url LIKE ? ORDER BY c.id DESC LIMIT 40",
            (f"{RETAIL}api/game/v1/public/characters/{run['char']}%",),
        ):
            d = js(rs)
            ch = d.get("character") if isinstance(d, dict) else None
            if isinstance(ch, dict) and ch.get("level"):
                run["levels"].append(ch["level"])
                break

    rungs_out = []
    chest_tiers = collections.defaultdict(collections.Counter)
    chest_level_ok = chest_level_n = 0
    for score in LADDER:
        obs = []
        for run in runs.values():
            rw = run["rungs"].get(score)
            if rw is None or not run["levels"]:
                continue
            level = run["levels"][0]
            for c in rw.get("chests") or []:
                chest_tiers[score][c.get("tier")] += 1
                chest_level_n += 1
                chest_level_ok += c.get("level") in run["levels"]
            obs.append({"level": level, "reward": rw})
        if chest_tiers.get(score):
            if len(chest_tiers[score]) != 1:
                sys.exit(f"rung {score} carries more than one chest tier: {dict(chest_tiers[score])}")
            rungs_out.append({"score": score, "kind": "chest",
                              "tier": next(iter(chest_tiers[score])),
                              "levelRule": "character", "observations": len(obs)})
            continue
        obs.sort(key=lambda o: (o["level"], json.dumps(o["reward"], sort_keys=True)))
        kinds = sorted({k for o in obs for k in o["reward"]})
        rungs_out.append({"score": score, "kind": "+".join(kinds) or "empty",
                          "observations": len(obs), "results": obs})

    end_obs = []
    for run in runs.values():
        if run["end"] is None or not run["levels"]:
            continue
        rw = run["end"]
        reached = [s for s in LADDER if s < max(run["rungs"] or [0])]
        end_obs.append({
            "level": run["levels"][0],
            "initialPlayerLevel": run["ipl"],
            "startFloor": run["startFloor"],
            "highestRungReached": max(reached) if reached else 0,
            "package": {"stackableItems": rw["stackableItems"]} if rw.get("stackableItems") else None,
            "gear": [i.get("itemTemplateId") for i in rw.get("items") or []],
            "paidGold": bool(rw.get("currencies")),
        })
    end_obs.sort(key=lambda o: (o["level"], json.dumps(o["package"], sort_keys=True)))

    out = {
        "_meta": {
            "description": "Retail Abyss rewards: the ten-rung score gauge and the /end package, "
                           "one observation per retail RUN, each tagged with the character level it was made at.",
            "authoritative": "every reward appeared verbatim in a retail /abysses response; nothing is interpolated",
            "runs": len(runs),
            "runsWithoutLevel": sum(1 for r in runs.values() if not r["levels"]),
            "chestLevelEqualsCharacterLevel": [chest_level_ok, chest_level_n],
            "chestTierPerRung": {str(k): dict(v) for k, v in chest_tiers.items()},
            "selection": "the consumer draws from the observations in the character's level band "
                         "(blades_lib::features::abyss_rewards::LEVEL_BANDS); a band with none falls back "
                         "to the nearest band that has some",
            "generator": "scripts/mine-abyss-rewards.py",
        },
        "rungs": rungs_out,
        "endPackage": {"observations": end_obs},
    }
    with open(a.out, "w") as f:
        json.dump(out, f, indent=1, sort_keys=False)
    print(f"runs {len(runs)}; rung observations "
          f"{ {r['score']: r['observations'] for r in rungs_out} }; /end {len(end_obs)}; "
          f"chest level == character level {chest_level_ok}/{chest_level_n}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
