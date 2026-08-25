# Deploying the newblades arena server

Standardised deploy / start / stop via **`deploy/arena.sh`** (a thin wrapper over
`docker-compose.arena.yml`). The stack: `arena-server` (the Rust game server —
blades.bgs.services REST + matchmaking + the live `rusty_enet` UDP arena host),
`arena-db` (tuned Postgres), `arena-migrate` (one-shot, idempotent schema apply).
Both app containers are memory-capped (256 MB) and cgroup-isolated so they can't
starve a co-located stack.

## Reading the logs

Container logs go to the **host journal**, not `docker logs`, because
`docker compose up -d` recreates containers and destroys their json-file logs.
That cost us the entire first human-vs-human arena session: it was investigated a
day later and every combat line was already gone, wiped by the deploy that
followed. Only the Postgres rows survived, and they do not record what the engine
decided.

```sh
journalctl CONTAINER_NAME=arena-server --since '2026-08-25 20:00'
journalctl CONTAINER_NAME=arena-server -f            # follow a live match
journalctl CONTAINER_NAME=arena-server | grep -E 'matchmaker:|combat:'
journalctl CONTAINER_NAME=arena-db --since today     # deploy-time schema errors
```

`docker logs arena-server` still works for the CURRENT container; it is the
history that now lives elsewhere. The journal on this box is persistent
(`/var/log/journal`) and rotates itself.

Set `ARENA_LOG_DRIVER=json-file` in `arena.env` to opt out on a host without
persistent journald — the logs then die with the container again.


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

# from any checkout, over ssh:
deploy/arena.sh static --dry-run  # what a static-data sync would change
deploy/arena.sh static            # ship deploy/static/ + restart the server

# on the SERVER box (repo dir; deploy/arena.env present):
deploy/arena.sh up              # start db → migrate → server (idempotent)
deploy/arena.sh status          # container state + health
deploy/arena.sh logs arena-server
deploy/arena.sh verify          # REST reachability + ps
deploy/arena.sh restart
deploy/arena.sh down            # stop (keeps the arena-db-data volume)
deploy/arena.sh migrate         # re-run the idempotent migration if needed
```
(Env overrides: `ARENA_ENV`, `ARENA_BOX`, `ARENA_BOX_DIR`, `ARENA_SSH_KEY`.)

## Shipping game data (`deploy/arena.sh static`)

`deploy/static/*.json` is mounted read-only at `/data/static` and read **once, at
startup**. `sync` deliberately skips `deploy/` — `arena.env` lives there — so for
a long time nothing shipped this directory at all and it drifted: files sat in
git for weeks while the box logged

```
[static] no "/data/static/recipe_crafting_types.json": No such file or directory; using default
```

once at boot and then served the fallback silently. Each missing file degrades
one feature (crafting-bench names, sell prices, enchant values, repair costs)
without erroring, so nothing surfaces it. `static` is the step that was missing.

```bash
deploy/arena.sh static --dry-run   # itemises what would change; touches nothing
deploy/arena.sh static             # rsync, then `docker restart arena-server`
```

Run the dry run first. Three things about it are deliberate:

- **It restarts the server.** A static-data sync without a restart is a no-op,
  which is exactly the failure mode being fixed. It is a plain `docker restart`
  on the running container, not a compose `up`: the compose file root actually
  runs is `/etc/newblades/docker-compose.arena.yml`, not the repo copy.
- **No `--delete`, ever.** The box's `deploy/static` is not a mirror of git.
  `bundles.blades.bgs.services/` is a 1.1 GB asset mirror maintained in place by
  blades-capture's `scripts/bundle-mirror.py` (excluded outright, since git holds
  only a `.gitkeep` for it), and files such as `default_town.json` and
  `item_durability.json` are generated straight onto the box. `--delete` would
  erase all of it on the first run.
- **Overwrites are kept** under `deploy/static-backup/<UTC timestamp>/`, outside
  the mounted directory. git is not reliably the newer side — some of these
  files are produced by generators that write to the box — so an overwrite can
  be a downgrade, and `-i` names every file it touched.

After the sync the command prints any `[static]` lines the restarted server
logged. No output means every file loaded.

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
