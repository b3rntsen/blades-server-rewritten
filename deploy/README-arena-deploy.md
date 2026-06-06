# Deploying the newblades arena server

One-command-away deploy of the playable arena server (#NB-4) + its Postgres.
**Held** intentionally: the current prod box is 1.9 GB shared with another prod
stack — deploy here only with the memory caps below, or after a RAM upgrade.

## What you get
- `arena-server` — the Rust game server: blades.bgs.services REST + matchmaking +
  the live `rusty_enet` UDP arena host. Memory-capped at 256 MB.
- `arena-db` — a tuned, 256 MB-capped Postgres (the 4-JSONB character store the
  `/arena` Transfer import writes to).
- `arena-migrate` — one-shot, idempotent: applies the diesel migrations on a
  fresh DB so the server finds its tables.

Both app containers are cgroup-isolated (`mem_limit`) → they cannot starve a
co-located stack; the host's 2 GB swap is the backstop.

## Build the image OFF the box
A release build is memory-hungry and will OOM the 1.9 GB box. Build it anywhere
with ≥ ~4 GB RAM, then ship the image:

    # on a build machine (x86_64 target = the prod box):
    docker build -t blades-arena-server .
    docker save blades-arena-server | gzip > arena-server.img.gz
    # ship + load on the box:
    scp -i ~/.ssh/twitter-bookmarks-key.pem arena-server.img.gz ec2-user@newblades.dethele.com:/tmp/
    ssh … 'gunzip -c /tmp/arena-server.img.gz | sudo docker load'

(If your build machine is arm64, add `--platform linux/amd64` to `docker build`.)

## Configure + run (on the box)
    cp deploy/arena.env.example deploy/arena.env   # fill in ARENA_DB_PASSWORD, ARENA_IMPORT_TOKEN
    sudo docker compose --env-file deploy/arena.env -f docker-compose.arena.yml up -d
    # arena-migrate runs once; arena-server waits for it, then starts.

Verify:
    sudo docker compose -f docker-compose.arena.yml ps
    sudo docker logs arena-server --tail 20      # "arena-enet: live host bound udp/7777"
    curl -fsS http://127.0.0.1:8087/blades.bgs.services/api/status   # or any REST route

## Wire the web (makes /arena Transfer work)
On the newblades-web container set (matching the token above):
    ARENA_SERVER_URL=http://arena-server:8080      # reachable via edge_net by name
    ARENA_IMPORT_TOKEN=<same as arena.env>
…then redeploy web (`scripts/deploy.sh web`). Until then, Transfer returns 503
("ARENA_IMPORT_TOKEN not set") — the correct not-wired state.

## Reachability
- REST is bound to `127.0.0.1:8087` on the host (the web reaches the server over
  `edge_net`, not this port) — not public.
- The arena **UDP** port (7777) is what clients dial. The handshake is not yet
  authenticated (Ed25519 swap = #NB-6), so firewall it to just you (security
  group / iptables) until then.

## Game data
`deploy/static/parsed.json` is a STUB (empty tables) — the server boots and the
arena match path (matchmaking + UDP + FSM) is fully in-memory, so it plays. But
character/dungeon/quest REST data is empty until a real `parsed.json` is
generated (`script/data_parser/main.py <decompiled-unity-data> parsed.json`).
Drop the real file at `deploy/static/parsed.json` and restart arena-server.

## Notes
- `deploy/arena.env` holds secrets — keep it out of git.
- Data persists in the `arena-db-data` volume across restarts/reboots
  (`restart: always`). Reinstall = rerun the build+load+up steps above.
