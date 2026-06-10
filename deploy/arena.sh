#!/usr/bin/env bash
#
# arena.sh — standardised lifecycle for the newblades arena server.
#
# WHERE TO RUN:
#   build / push    → on a BUILD host with Docker + >=4GB RAM (NOT the 1.9GB box;
#                     a release build OOMs it). build → image; push → load on box.
#   up/down/etc.    → on the SERVER box, from the repo dir (needs
#                     docker-compose.arena.yml + deploy/arena.env present).
#
# Subcommands:
#   build           docker-build the arena-server image
#   push            save the image + docker-load it on the box over ssh
#   sync            rsync source to the box for an in-place `build` (deploy/ kept)
#   up | start      start the stack (arena-db → arena-migrate → arena-server); idempotent
#   down | stop     stop + remove containers (the arena-db-data volume is kept)
#   restart         down then up
#   status          container state + health
#   logs [svc]      follow logs (optionally one service: arena-server / arena-db)
#   migrate         re-run the idempotent DB migration (safe; no-op if applied)
#   verify          quick reachability probe (REST port) + container state
#
# Config (env overrides): ARENA_ENV (default deploy/arena.env), ARENA_BOX,
# ARENA_SSH_KEY. Secrets (ARENA_DB_PASSWORD, ARENA_IMPORT_TOKEN, …) live in
# deploy/arena.env — see deploy/arena.env.example.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE="$ROOT/docker-compose.arena.yml"
ENVFILE="${ARENA_ENV:-$ROOT/deploy/arena.env}"
IMAGE="blades-arena-server"
BOX="${ARENA_BOX:-ec2-user@newblades.dethele.com}"
SSH_KEY="${ARENA_SSH_KEY:-$HOME/.ssh/twitter-bookmarks-key.pem}"

dc() {
  [ -f "$ENVFILE" ] || {
    echo "missing $ENVFILE — cp deploy/arena.env.example deploy/arena.env and fill it" >&2
    exit 1
  }
  sudo docker compose --env-file "$ENVFILE" -f "$COMPOSE" "$@"
}

usage() { sed -n '3,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

cmd="${1:-}"
[ $# -gt 0 ] && shift || true
case "$cmd" in
  build)        docker build -t "$IMAGE" "$ROOT" ;;
  push)         docker save "$IMAGE" | gzip | ssh -i "$SSH_KEY" "$BOX" 'gunzip | sudo docker load' ;;
  sync)
    # rsync the SOURCE to the box's compose dir for an in-place `build` (the box
    # is not a git repo). Never touches deploy/ — its static data + arena.env stay.
    rsync -az --exclude='target/' -e "ssh -i $SSH_KEY" \
      "$ROOT"/Cargo.toml "$ROOT"/Cargo.lock "$ROOT"/Dockerfile "$ROOT"/docker-compose.arena.yml \
      "$ROOT"/server "$ROOT"/blades_lib "$ROOT"/arena_proto "$ROOT"/migrations \
      "$BOX:/home/ec2-user/blades-server/" \
      && echo "synced source → $BOX (deploy/ untouched)" ;;
  up|start)     dc up -d && echo "started — check: $0 status" ;;
  down|stop)    dc down ;;
  restart)      dc down; dc up -d ;;
  status)       dc ps ;;
  logs)         dc logs -f --tail=100 "$@" ;;
  migrate)      dc up -d arena-db && dc run --rm arena-migrate ;;
  verify)
    printf 'REST :8087 '
    curl -sS -m 5 -o /dev/null -w '→ HTTP %{http_code}\n' \
      http://127.0.0.1:8087/blades.bgs.services/api/status 2>/dev/null \
      || echo '→ unreachable'
    dc ps ;;
  ""|-h|--help|help) usage ;;
  *) echo "unknown: $cmd" >&2; usage; exit 1 ;;
esac
