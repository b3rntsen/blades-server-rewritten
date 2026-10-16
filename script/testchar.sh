#!/usr/bin/env bash
# Operator tool for TEST characters (server/src/testchar.rs).
#
# Runs ON the arena box (it talks to 127.0.0.1:8087 and the arena-db container).
# Commands:
#
#   alts   <userId>                                       read-only: the account's alts
#   mark   <userId> <altUuid> <expectedName> <actor> [note]
#   level  <altUuid> <level 1-100> <actor>
#   list                                                  read-only: marked alts + audit
#   verify <altUuid>                                      read-only: marker, ratings, boards
#
# From a workstation, ssh hands the arguments to the REMOTE SHELL, so quote each
# one with printf %q (a name or note with spaces or `$(...)` would otherwise be
# split or executed on the box):
#
#   tc() { ssh -i KEY ec2-user@HOST "bash -s -- $(printf '%q ' "$@")" < script/testchar.sh; }
#   tc alts e05030f3-74a5-5295-8358-8f83dabccefc
#   tc mark <userId> <altUuid> 'Name As Shown' 'claude (ticket duty)' 'loot tuning #536'
#
# Exits non-zero on a usage error or any non-2xx answer.
#
# Reads are SELECT-only psql or GETs. The ONLY writes are `mark` and `level`, and
# both go through the server's own routes, which refuse anything that is not the
# user's live, unplayed (mark) / marked (level) alt and audit every change in
# `test_character_audit`. Nothing here edits character JSON by hand.
#
# The import token is read from /etc/newblades/arena.env (root-only) with sudo and
# handed to curl as a header file on a pipe, so it never appears in argv or output.

set -euo pipefail

API="${ARENA_API:-http://127.0.0.1:8087}/blades.bgs.services/api/dev/v1"
ENV_FILE="${ARENA_ENV_FILE:-/etc/newblades/arena.env}"
UUID_RE='^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'

die() { echo "error: $*" >&2; exit 2; }
need_uuid() { [[ "$2" =~ $UUID_RE ]] || die "$1 must be a uuid, got '$2'"; }

psql_ro() {
  # One read-only transaction: a typo here cannot write.
  docker exec -i arena-db psql -U blades -d blades -v ON_ERROR_STOP=1 -P pager=off \
    -c 'SET default_transaction_read_only = on' "$@"
}

token_header() {
  local reader=(sed -n 's/^ARENA_IMPORT_TOKEN=//p' "$ENV_FILE") t
  [[ -r "$ENV_FILE" ]] || reader=(sudo -n "${reader[@]}")
  t="$("${reader[@]}" | tr -d "\"'\r")"
  [[ -n "$t" ]] || die "ARENA_IMPORT_TOKEN not found in $ENV_FILE"
  printf 'X-Import-Token: %s\n' "$t"
}

api() { # METHOD PATH [JSON] — prints the body and status; fails unless 2xx
  local method="$1" path="$2" body="${3:-}" out
  if [[ -n "$body" ]]; then
    out="$(curl -sS -X "$method" -H @<(token_header) -H 'Content-Type: application/json' \
      --data-binary "$body" -w '\nHTTP %{http_code}' "$API$path")" || return 1
  else
    out="$(curl -sS -X "$method" -H @<(token_header) -w '\nHTTP %{http_code}' "$API$path")" || return 1
  fi
  printf '%s\n' "$out"
  [[ "$(tail -n1 <<<"$out")" =~ ^HTTP\ 2[0-9][0-9]$ ]]
}

json_str() { python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$1"; }

cmd="${1:-}"; shift || true
case "$cmd" in
  alts)
    [[ $# -eq 1 ]] || die "usage: alts <arenaUserId>"
    need_uuid userId "$1"
    psql_ro -c "
      SELECT 'LIVE' AS state, c.source_alt_uuid AS alt_uuid, c.id AS character_id,
             c.character->>'name' AS name, (c.character->>'level')::int AS level,
             (t.alt_uuid IS NOT NULL) AS is_test
        FROM characters c LEFT JOIN test_characters t ON t.alt_uuid = c.source_alt_uuid
       WHERE c.user_id = '$1'
      UNION ALL
      SELECT 'saved', s.source_alt_uuid, s.character_id, s.name, s.level,
             (t.alt_uuid IS NOT NULL)
        FROM (SELECT DISTINCT ON (v.source_alt_uuid) v.*
                FROM character_versions v
               WHERE v.user_id = '$1'
                 AND v.source_alt_uuid IS DISTINCT FROM
                     (SELECT source_alt_uuid FROM characters WHERE user_id = '$1')
               ORDER BY v.source_alt_uuid, v.saved_at DESC) s
        LEFT JOIN test_characters t ON t.alt_uuid = s.source_alt_uuid
       ORDER BY 1, 2;"
    ;;
  mark)
    [[ $# -ge 4 && $# -le 5 ]] || die "usage: mark <arenaUserId> <altUuid> <expectedName> <actor> [note]"
    need_uuid userId "$1"; need_uuid altUuid "$2"
    note='null'; [[ $# -eq 5 ]] && note="$(json_str "$5")"
    api POST /test-characters/mark \
      "{\"userId\":\"$1\",\"altUuid\":\"$2\",\"expectedName\":$(json_str "$3"),\"actor\":$(json_str "$4"),\"note\":$note}"
    ;;
  level)
    [[ $# -eq 3 ]] || die "usage: level <altUuid> <level 1-100> <actor>"
    need_uuid altUuid "$1"
    [[ "$2" =~ ^[0-9]{1,3}$ ]] || die "level must be a number, got '$2'"
    api POST "/test-characters/$1/level" "{\"level\":$2,\"actor\":$(json_str "$3")}"
    ;;
  list)
    api GET /test-characters
    ;;
  verify)
    [[ $# -eq 1 ]] || die "usage: verify <altUuid>"
    need_uuid altUuid "$1"
    echo "== marker, live row and rating rows (expect: marked, 0 rating rows)"
    psql_ro -c "
      SELECT (SELECT count(*) FROM test_characters WHERE alt_uuid = '$1') AS marked,
             (SELECT count(*) FROM arena_h2h_ratings WHERE alt_key = '$1') AS h2h_rows,
             (SELECT count(*) FROM arena_h2h_season_ratings WHERE alt_key = '$1') AS h2h_season_rows,
             (SELECT count(*) FROM arena_season_standings WHERE source_alt_uuid = '$1') AS standings,
             (SELECT count(*) FROM arena_season_awards WHERE source_alt_uuid = '$1') AS awards;" \
      -c "SELECT action, actor, detail, to_timestamp(at) AS at
            FROM test_character_audit WHERE alt_uuid = '$1' ORDER BY id;"
    echo "== boards: rows carrying this alt's id or any (Test) name (expect: none)"
    alt_lc="$(tr 'A-F' 'a-f' <<<"$1")"
    for board in "arena-ranking/game-top?limit=100" "arena-ranking/game-guilds?limit=100" \
                 "arena-ranking/h2h-top?limit=100&season=current" \
                 "arena-ranking/h2h-top?limit=100&season=all" \
                 "arena-ranking/h2h-guilds?limit=100&season=current"; do
      out="$(api GET "/$board")" || die "could not read $board"
      hits="$( { grep -o '"name":"[^"]*(Test)"' <<<"$out"; grep -o "\"$alt_lc\"" <<<"$out"; } || true)"
      echo "$board: ${hits:-none}"
    done
    ;;
  *)
    sed -n '2,20p' "$0" 2>/dev/null || true
    die "unknown command '${cmd}'"
    ;;
esac
