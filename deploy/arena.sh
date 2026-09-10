#!/usr/bin/env bash
#
# arena.sh — standardised lifecycle for the newblades arena server.
#
# WHERE TO RUN:
#   build / push    → on a BUILD host with Docker + >=4GB RAM (NOT the 1.9GB box;
#                     a release build OOMs it). build → image; push → load on box.
#   sync / static   → from a checkout on your own machine; both push to the box
#                     over ssh and need nothing installed there.
#   up/down/etc.    → on the SERVER box, from the repo dir (needs
#                     docker-compose.arena.yml + deploy/arena.env present).
#
# Subcommands:
#   build           docker-build the arena-server image
#   push            save the image + docker-load it on the box over ssh
#   sync            rsync source to the box for an in-place `build` (deploy/ kept)
#   static          rsync deploy/static/ (the game data the server reads at
#                   startup) to the box, then restart arena-server so it takes.
#                   `static --dry-run` shows what would change and touches nothing
#   up | start      start the stack (arena-db → arena-migrate → arena-server); idempotent
#   down | stop     stop + remove containers (the arena-db-data volume is kept)
#   restart         down then up
#   status          container state + health
#   logs [svc]      follow logs (optionally one service: arena-server / arena-db)
#   migrate         re-run the idempotent DB migration (safe; no-op if applied)
#   verify          quick reachability probe (REST port) + container state
#
# Config (env overrides): ARENA_ENV (default deploy/arena.env), ARENA_BOX,
# ARENA_BOX_DIR, ARENA_SSH_KEY. Secrets (ARENA_DB_PASSWORD, ARENA_IMPORT_TOKEN,
# …) live in deploy/arena.env — see deploy/arena.env.example.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE="$ROOT/docker-compose.arena.yml"
ENVFILE="${ARENA_ENV:-$ROOT/deploy/arena.env}"
IMAGE="blades-arena-server"
BOX="${ARENA_BOX:-ec2-user@newblades.dethele.com}"
BOX_DIR="${ARENA_BOX_DIR:-/home/ec2-user/blades-server}"
SSH_KEY="${ARENA_SSH_KEY:-$HOME/.ssh/twitter-bookmarks-key.pem}"

dc() {
  [ -f "$ENVFILE" ] || {
    echo "missing $ENVFILE — cp deploy/arena.env.example deploy/arena.env and fill it" >&2
    exit 1
  }
  sudo docker compose --env-file "$ENVFILE" -f "$COMPOSE" "$@"
}

# The one rsync invocation `static` uses, so the dry run and the real run cannot
# drift apart — a preview that does not describe the thing it previews is worse
# than no preview. Extra flags (--dry-run, --backup…) are passed by the caller.
#
# NOT --delete, and never add it. The box's deploy/static is not a mirror of
# git: bundles.blades.bgs.services/ is a 1.1 GB asset mirror that
# blades-capture's scripts/bundle-mirror.py maintains in place, and
# default_town.json is written there by seed-default-town.py. --delete would
# erase all of it on the first run. `sync` does not --delete either.
#
# The bundle mirror is excluded outright rather than merely spared, because git
# holds nothing but a .gitkeep for it: including it would walk 1673 remote files
# every run in order to transfer nothing.
static_rsync() {
  rsync -az --itemize-changes "$@" \
    --exclude='bundles.blades.bgs.services/' \
    -e "ssh -i $SSH_KEY" \
    "$ROOT/deploy/static/" "$BOX:$BOX_DIR/deploy/static/"
}

# Print the header comment block. Stops at the first non-comment line rather
# than a fixed line number, so adding a subcommand above cannot silently start
# printing code (the old '3,30p' had already begun to).
usage() {
  awk 'NR >= 3 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "${BASH_SOURCE[0]}"
}

cmd="${1:-}"
[ $# -gt 0 ] && shift || true
case "$cmd" in
  build)
    docker build -t "$IMAGE" "$ROOT"
    # Cap build-cache growth: arena builds run --no-cache (cache is never reused),
    # so every build leaves ~7min of dangling layers. Left unpruned this reached
    # 116 GB and filled the prod disk (2026-07). Drop cache older than 48h after
    # each build — keeps same-session iteration fast, caps accumulation by time.
    # A weekly blades-docker-prune.timer on the box is the backstop for on-box
    # `docker build` runs that bypass this subcommand.
    docker builder prune -f --filter until=48h >/dev/null 2>&1 || true
    ;;
  push)         docker save "$IMAGE" | gzip | ssh -i "$SSH_KEY" "$BOX" 'gunzip | sudo docker load' ;;
  sync)
    # rsync the SOURCE to the box's compose dir for an in-place `build` (the box
    # is not a git repo). Never touches deploy/ — its static data + arena.env stay.
    rsync -az --exclude='target/' -e "ssh -i $SSH_KEY" \
      "$ROOT"/Cargo.toml "$ROOT"/Cargo.lock "$ROOT"/Dockerfile "$ROOT"/docker-compose.arena.yml \
      "$ROOT"/server "$ROOT"/blades_lib "$ROOT"/arena_proto "$ROOT"/migrations \
      "$BOX:$BOX_DIR/" \
      && echo "synced source → $BOX (deploy/ untouched — use \`$0 static\` for deploy/static)" ;;
  static)
    # `sync` skips deploy/ on purpose (arena.env lives there), so for months
    # nothing shipped deploy/static — the game data the server reads at startup
    # from the ./deploy/static:/data/static:ro bind mount. It rotted silently:
    # four committed files were absent on the box and the server logged
    #   [static] no "/data/static/recipe_crafting_types.json": …; using default
    # once at boot and then served the fallback forever. This is that missing
    # step, so it stops being a hand-rsync someone has to remember.
    dryrun=0
    if [ "${1:-}" = "--dry-run" ]; then dryrun=1; shift; fi
    [ $# -eq 0 ] || { echo "usage: $0 static [--dry-run]" >&2; exit 1; }

    if [ "$dryrun" = 1 ]; then
      static_rsync --dry-run
      echo "dry run — the box was not touched. Re-run without --dry-run to apply."
      exit 0
    fi

    # Overwrites are kept, outside the mounted directory so the server never
    # sees them. git is not reliably the newer side: some of these files are
    # generated straight onto the box (build-gifts-static.py and friends write
    # into deploy/static), so a sync can legitimately be a downgrade. -i above
    # names every file it touched; this makes undoing one a `cp`.
    stamp="$(date -u +%Y%m%dT%H%M%SZ)"
    static_rsync --backup --backup-dir="$BOX_DIR/deploy/static-backup/$stamp"
    echo "synced deploy/static/ → $BOX (no --delete; any overwrite is in deploy/static-backup/$stamp)"

    # Inert until the server restarts — it reads these once, at startup. Plain
    # `docker restart` re-execs the existing container, so it re-reads the bind
    # mount without touching the image, deploy/arena.env, or the compose file
    # (the one root actually runs is /etc/newblades/docker-compose.arena.yml,
    # NOT the repo copy — `dc` here would use the wrong one).
    ssh -i "$SSH_KEY" "$BOX" 'sudo docker restart arena-server >/dev/null && echo "restarted arena-server"'
    sleep 3
    echo "--- [static] complaints since the restart (no output = every file loaded) ---"
    ssh -i "$SSH_KEY" "$BOX" \
      'sudo docker logs --since 60s arena-server 2>&1 | grep -F "[static]" || true' ;;
  up|start)     dc up -d && echo "started — check: $0 status" ;;
  down|stop)    dc down ;;
  restart)      dc down; dc up -d ;;
  status)       dc ps ;;
  logs)         dc logs -f --tail=100 "$@" ;;
  migrate)      dc up -d arena-db && dc run --rm arena-migrate ;;
  verify)
    printf 'REST + PostgreSQL :8087 '
    curl -fsS -m 5 -o /dev/null -w '→ HTTP %{http_code}\n' \
      http://127.0.0.1:8087/healthz
    dc ps ;;
  ""|-h|--help|help) usage ;;
  *) echo "unknown: $cmd" >&2; usage; exit 1 ;;
esac
