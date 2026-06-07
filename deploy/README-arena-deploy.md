# Deploying the newblades arena server

Standardised deploy / start / stop via **`deploy/arena.sh`** (a thin wrapper over
`docker-compose.arena.yml`). The stack: `arena-server` (the Rust game server —
blades.bgs.services REST + matchmaking + the live `rusty_enet` UDP arena host),
`arena-db` (tuned Postgres), `arena-migrate` (one-shot, idempotent schema apply).
Both app containers are memory-capped (256 MB) and cgroup-isolated so they can't
starve a co-located stack.

## Prerequisites
- **Build host** with Docker + ≥ 4 GB RAM (a release build OOMs the 1.9 GB prod
  box — build off-box, ship the image).
- **The prod box has enough RAM** for the stack (the RAM upgrade) before enabling.
- `deploy/arena.env` (copy from `deploy/arena.env.example`, fill the secrets).
- **Game data** (`deploy/static/parsed.json`): the committed file is a STUB
  (empty) — the server boots and **arena/PvP plays fine** (its path is in-memory),
  but **quests/dungeons return empty** until you drop a real `parsed.json`
  (generate with `script/data_parser/main.py <decompiled-unity-data> parsed.json`).

## Lifecycle (deploy/arena.sh)
```
# on the BUILD host (Docker + >=4GB):
deploy/arena.sh build           # build the arena-server image
deploy/arena.sh push            # docker save | ssh → docker load on the box

# on the SERVER box (repo dir; deploy/arena.env present):
deploy/arena.sh up              # start db → migrate → server (idempotent)
deploy/arena.sh status          # container state + health
deploy/arena.sh logs arena-server
deploy/arena.sh verify          # REST reachability + ps
deploy/arena.sh restart
deploy/arena.sh down            # stop (keeps the arena-db-data volume)
deploy/arena.sh migrate         # re-run the idempotent migration if needed
```
(Env overrides: `ARENA_ENV`, `ARENA_BOX`, `ARENA_SSH_KEY`.)

## Wire the web (makes /arena Transfer work)
On the `newblades-web` container set (same token as `arena.env`):
```
ARENA_SERVER_URL=http://arena-server:8080     # reachable by name over edge_net
ARENA_IMPORT_TOKEN=<same as ARENA_IMPORT_TOKEN in arena.env>
```
then `scripts/deploy.sh web`. Until then `/arena` Transfer returns 503 (the
correct not-wired state).

## Enable arena play routing (capture platform — separate repo)
These are OFF by default. Turn on so a WG client (the com.dethele.newblades APK)
plays on our server instead of Bethesda — all WG-confined:
1. **Arena redirect** (HTTPS): set `ARENA_REDIRECT=1` (+ `ARENA_HOST`/`ARENA_PORT`)
   in the mitmproxy env and restart `blades-mitmproxy` — re-points
   `blades.bgs.services` auth/game/matchmaking/rms to our server (capture CA reused).
2. **Region-ping responder** (the latency phase): `deploy.sh scripts && deploy.sh
   systemd` then `sudo systemctl enable --now blades-arena-ping-responder` —
   answers the GameLift latency probes on `wg0:80` so "Searching" doesn't stall.
3. Firewall the arena **UDP** port (`ARENA_UDP_PORT`, default 7777) to the WG
   subnet until the Ed25519/handshake interop is finalised.

## Reachability
REST is bound to `127.0.0.1:8087` on the host (the web reaches the server over
`edge_net` by name, not this port). The UDP arena port is what clients dial.

## Notes
- `deploy/arena.env` holds secrets — gitignored, never commit.
- Data persists in the `arena-db-data` volume across restarts/reboots.
- Reinstall = rerun `build` → `push` → `up` (+ the routing enable steps).
- The arena UDP handshake is the proven op-0x38 format (`docs/arena-protocol-spec.md`
  §4.1) — confirm the server speaks it before real-client testing.
