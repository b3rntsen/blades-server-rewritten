#!/usr/bin/env python3
"""Drive one character through the whole progression loop against a running server.

The unit tests check pure functions against captured retail observations. This
checks the things they structurally cannot: a real HTTP surface, static data
loaded off disk, a real database, and the interactions between them. It found
three bugs the whole test suite was green through —

  * the town-job board still paid the OLD quest XP, because five call sites asked
    `QuestLevelScaling::default()`, an empty table whose `given_xp` falls through
    to `100 * enemyLevel`. A fresh character's board is entirely jobs, so it was
    the first thing a new player saw;
  * every dungeon on the ordinary quest path reported `level: 1, seed: 54321`;
  * a freshly placed building announced itself as `UPGRADING` where retail says
    `BUILDING`.

Run it:

    createdb blades_drive
    DATABASE_URL=postgres://$USER@localhost/blades_drive diesel migration run
    cargo run --release -p server -- run \
        --connection-string postgres://$USER@localhost/blades_drive \
        --host 127.0.0.1 --port 18087 --static-data ./deploy/static &
    python3 script/drive_journey.py blades_drive

It writes to the database through `psql` to bank experience and stock build
materials — reaching a state a real player reaches over days. Point it at a
throwaway database, never a real one.
"""

import json, subprocess, sys, urllib.request, urllib.error, uuid

BASE = "http://127.0.0.1:18087/blades.bgs.services/api/game/v1/public"
AUTH = "http://127.0.0.1:18087/blades.bgs.services/api/authentication/v1/public"
GOLD = "f8d27767-a85e-4fd6-a5bb-bf8a13d0daa2"
GEMS = "470c8f58-a8dd-4c07-8c92-843b785e1139"
DB = sys.argv[1] if len(sys.argv) > 1 else "blades_journey_58324"
NULL = object()
token = None
fails = []
notes = []

def call(method, path, body=None, expect=200, label=None, base=BASE):
    url = base + path
    data = None if body is None else json.dumps(None if body is NULL else body).encode()
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    if token:
        req.add_header("Authorization", f"blade_v1={token}")
    try:
        with urllib.request.urlopen(req) as r:
            code, raw = r.status, r.read()
    except urllib.error.HTTPError as e:
        code, raw = e.code, e.read()
    try:
        out = json.loads(raw) if raw else None
    except Exception:
        out = {"__raw": raw[:200].decode("utf8", "replace")}
    tag = label or f"{method} {path}"
    ok = code == expect
    print(f"  {'ok  ' if ok else 'FAIL'} {code:3d} (want {expect})  {tag}")
    if not ok:
        fails.append(tag)
        print("        ->", json.dumps(out)[:300])
    return out

def check(cond, what):
    print(f"  {'ok  ' if cond else 'FAIL'}       {what}")
    if not cond:
        fails.append(what)

def sql(q):
    return subprocess.run(["psql", "-tA", "-d", DB, "-c", q],
                          capture_output=True, text=True).stdout.strip()

def bal(wallet, cur=GOLD):
    for e in wallet or []:
        if e.get("currencyId") == cur:
            return e["balance"]
    return None

print("\n=== 1. a brand-new player ===")
s = call("POST", "/auth/anon", {"userId": None, "deviceId": f"drive-{uuid.uuid4()}", "platform": "gp"},
         label="anon login", base=AUTH)
token = s["session"]["token"]
chars = call("GET", "/characters")
ch = chars["characters"][0]
cid = ch["id"]
print(f"        starter character {cid}: level {ch['level']}, {ch['experience']} xp")
check(ch["level"] == 1, "a new player starts at level 1")

print("\n=== 2. the quest board ===")
q = call("POST", f"/characters/{cid}/quests", NULL)
spawns = []
for gd in q.get("dungeonGeneratedDataList") or []:
    for group in (gd.get("enemyGeneratedData") or {}).values():
        for spawner in group:
            for sp in (spawner if isinstance(spawner, list) else [spawner]):
                spawns.append((sp.get("enemyLevel"), sp.get("givenXP")))
print(f"        {len(q.get('quests',[]))} quests, {len(q.get('jobs',[]))} jobs, "
      f"{len(q.get('gameEventQuests',[]))} events, {len(spawns)} spawns")
pairs = sorted({s for s in spawns})
print(f"        (enemyLevel, givenXP) pairs: {pairs[:10]}")
# The old formula was 100 * enemyLevel. Retail's level-1 enemy is worth 11.
bad = [(l, x) for l, x in spawns if l and x == 100 * l and x > 20]
check(not bad, f"no spawn still uses the 100*level formula (offenders: {sorted(set(bad))[:6]})")
check(all(x is None or x <= 300 for _, x in spawns),
      "every spawn's XP is in retail's range (the table tops out at 284)")

print("\n=== 3. play a quest end to end ===")
playable = (q.get("quests") or []) + (q.get("jobs") or []) + (q.get("gameEventQuests") or [])
qid = playable[0]["questId"]
kind = "quest" if q.get("quests") else ("job" if q.get("jobs") else "event")
print(f"        playing a {kind}: {qid}")
call("POST", f"/characters/{cid}/quests/{qid}/accept", NULL, label="accept")
# Retail uploads the client-generated dungeon + its opaque initial state. The
# server stores both verbatim, so an empty blob exercises the same path.
ent = call("POST", f"/characters/{cid}/quests/{qid}/dungeons/current/enter",
           {"dungeonInstance": {"b64": ""}, "currentState": {"b64": ""}}, label="enter dungeon")
if isinstance(ent, dict) and isinstance(ent.get("dungeonStatus"), dict):
    ds = ent["dungeonStatus"]
    print(f"        dungeon level={ds.get('level')} seed={ds.get('seed')}")
    check(ds.get("seed") not in (54321, None), f"the seed is generated, not hardcoded ({ds.get('seed')})")
call("POST", f"/characters/{cid}/quests/{qid}/dungeons/current/exit", {}, label="exit dungeon")
done = call("POST", f"/characters/{cid}/quests/{qid}/complete", {"autocomplete": False}, label="complete")
if isinstance(done, dict) and isinstance(done.get("character"), dict):
    print(f"        reward {json.dumps(done.get('reward'))[:140]}")
    print(f"        now level {done['character']['level']}, {done['character']['experience']} xp")

print("\n=== 4. level up: the gate, then the spend ===")
me = call("GET", f"/characters/{cid}")
c = me.get("character", me)
print(f"        level {c['level']}, {c['experience']} xp (level 2 costs 50)")
if c["experience"] < 50:
    call("POST", f"/characters/{cid}/levelup", {"attribute": "STAMINA"}, expect=400,
         label="levelup refused below the threshold")
# Bank exactly enough and try again — this is the half the unit tests cannot reach,
# because it goes through the handler, the transaction and the wire.
sql(f"UPDATE characters SET character = jsonb_set(character, '{{experience}}', '57') WHERE id = '{cid}'")
up = call("POST", f"/characters/{cid}/levelup", {"attribute": "STAMINA"}, label="levelup granted")
if isinstance(up, dict) and isinstance(up.get("character"), dict):
    got = up["character"]
    print(f"        level {got['level']}, {got['experience']} xp left, "
          f"wallet keys={list(up.keys())}")
    check(got["level"] == 2, "the level advanced")
    check(got["experience"] == 7, f"57 - 50 = 7 xp left (retail's exact remainder), got {got['experience']}")
    check("wallet" in up, "level 2 pays 500 gold, so the wallet is echoed")
    check(up.get("wallet") is None or len(up["wallet"]) == 1,
          f"and only the currency it credited: {up.get('wallet')}")

print("\n=== 5. build the town ===")
town = call("GET", f"/characters/{cid}/towns/current")
t = town.get("town", town)
xp0 = t["levelInfo"]["experiencePoints"]
print(f"        town level {t['levelInfo']['level']}, {xp0} xp")

# Find a free segment and a building type we can afford.
seg = None
for d in t.get("districts") or []:
    segs = d.get("segments") or {}
    for sid, sv in (segs.items() if isinstance(segs, dict) else []):
        if not (sv.get("buildings") or {}):
            seg = sv.get("segmentGroupId") or sid
            break
    if seg:
        break
print(f"        free segment group: {seg}")
sql(f"UPDATE characters SET wallet = '[{{\"currencyId\":\"{GOLD}\",\"balance\":5000000}}]' WHERE id = '{cid}'")
# Building costs materials as well as gold. Stock every material the town tables
# name, so the run exercises the charge path rather than failing on an empty bag.
mats = set()
_bu = json.load(open("/Users/berntsen/Projects/blades-server-rewritten/.wt/journey/deploy/static/building_upgrades.json"))
for _b in _bu["buildings"].values():
    for _l in (_b.get("levels") or {}).values():
        mats.update((_l.get("buildInputs") or {}).keys())
        for _si in (_l.get("styleInputs") or {}).values():
            mats.update(k for k in _si if len(k) == 36)
stack = json.dumps([{"itemTemplateId": m, "count": 100000} for m in sorted(mats)])
sql("UPDATE characters SET inventory = jsonb_set(inventory, '{backpack,stackableItems}', '"
    + stack.replace("'", "''") + "'::jsonb) WHERE id = '" + cid + "'")
print(f"        stocked {len(mats)} build materials")

HOUSE = "a6a2de53-d65c-445a-8b55-d2a73c15b635"  # TownHall: placeable at town level 0
STYLE = "aa133662-053d-434e-8779-3f2a41d1271e"
placed = call("POST", f"/characters/{cid}/towns/current/buildings",
              {"buildingType": HOUSE, "styleId": STYLE, "segmentGroupId": seg,
               "startIndex": 0, "npcIndex": 0, "gemsPayment": False}, label="place the town hall")
if isinstance(placed, dict) and isinstance(placed.get("town"), dict):
    xp1 = placed["town"]["levelInfo"]["experiencePoints"]
    gold1 = bal(placed.get("wallet"))
    check(xp1 == xp0, f"placing pays NO town xp ({xp0} -> {xp1})")
    check(gold1 is not None and gold1 < 5000000, f"placing costs gold (now {gold1})")
    # The default town already has buildings, so match the one just placed by its
    # type and its under-construction state rather than taking whatever is last.
    bid = None
    state = None
    for d in placed["town"].get("districts") or []:
        segs = d.get("segments") or {}
        for sv in (segs.values() if isinstance(segs, dict) else segs):
            for k, b in (sv.get("buildings") or {}).items():
                if b.get("typeId") == HOUSE and b.get("constructionEnd"):
                    bid, state = k, b.get("state")
    print(f"        placed building state={state} (retail: BUILDING)")
    print(f"        new building {bid}")
    if bid:
        fin = call("POST", f"/characters/{cid}/towns/current/buildings/{bid}/complete",
                   {"speedUp": False}, label="complete the build")
        if isinstance(fin, dict) and isinstance(fin.get("town"), dict):
            xp2 = fin["town"]["levelInfo"]["experiencePoints"]
            check(xp2 > xp1, f"completing DOES pay town xp ({xp1} -> {xp2})")
            check(sorted(fin.keys()) == ["town"],
                  f"a plain completion answers with {{town}} alone, got {sorted(fin.keys())}")
        NEW_STYLE = "1d6696b3-963b-48d4-8924-d43505cb2807"
        st = call("POST", f"/characters/{cid}/towns/current/buildings/{bid}/styles/{NEW_STYLE}",
                  None, label="restyle")
        if isinstance(st, dict) and isinstance(st.get("town"), dict):
            xp3 = st["town"]["levelInfo"]["experiencePoints"]
            check(xp3 > xp2, f"restyling DOES pay town xp ({xp2} -> {xp3})")

print("\n=== 6. the rest of the loop still answers ===")
for m, path, body, label in [
    ("POST", f"/characters/{cid}/gameevents", NULL, "gameevents"),
    ("POST", f"/characters/{cid}/challenges", NULL, "challenges"),
    ("GET",  f"/characters/{cid}/wallets/current", None, "wallet"),
    ("GET",  f"/characters/{cid}/crafts", None, "crafts"),
    ("GET",  f"/characters/{cid}/globalshops/current", None, "global shop"),
    ("POST", f"/characters/{cid}/towns/current/rewards/current", NULL, "daily reward"),
    ("GET",  f"/characters/{cid}/dungeons", None, "dungeons"),
    ("GET",  "/characters", None, "character list"),
]:
    call(m, path, body, label=label)

print()
if fails:
    print(f"FAILURES ({len(fails)}):")
    for f in fails:
        print("  -", f)
else:
    print("no failures — the loop runs end to end")
sys.exit(1 if fails else 0)
