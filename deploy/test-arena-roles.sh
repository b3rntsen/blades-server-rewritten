#!/usr/bin/env bash
# Proves the arena-migrate ROLES step (H2, security review 2026-11-24) against a
# REAL Postgres: a throwaway cluster (initdb in a temp dir, TCP only, port
# 55432, superuser named `blades` like production), removed at the end.
#
#   deploy/test-arena-roles.sh            # needs initdb/pg_ctl/psql >= 15 on PATH
#
# It checks that, after the step, arena_runtime (logging in with its password,
# as arena-server does) can do every class of statement the server issues at
# runtime and at startup -- DML, sequences, LOCK TABLE, advisory locks, CREATE
# TABLE IF NOT EXISTS, CREATE INDEX on an existing table, CREATE OR REPLACE
# FUNCTION, ALTER TABLE ADD COLUMN -- and can no longer do what only a superuser
# can (COPY ... PROGRAM, pg_read_file, CREATE ROLE, ALTER SYSTEM, ...).
# The SQL under test is extracted from docker-compose.arena.yml, not copied.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
COMPOSE=docker-compose.arena.yml
WORK="$(mktemp -d)"
PGD="$WORK/pgdata"; PORT="${PORT:-55432}"
python3 - "$COMPOSE" > "$WORK/roles.sql" <<'PY'
import sys
y = open(sys.argv[1]).read()
body = y.split("<<'SQL'\n", 1)[1].split("\n        SQL\n", 1)[0]
lines = [l[8:] if l.startswith(" " * 8) else l for l in body.split("\n")]
print("\n".join(lines).replace("$$", "$"))
PY
cleanup() { pg_ctl -D "$PGD" -m immediate stop >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT
initdb -D "$PGD" -U blades --auth-local=trust --auth-host=scram-sha-256 >/dev/null
printf "host all blades 127.0.0.1/32 trust\nhost all all 127.0.0.1/32 scram-sha-256\n" > "$PGD/pg_hba.conf"
pg_ctl -D "$PGD" -o "-p $PORT -c unix_socket_directories= -c listen_addresses=127.0.0.1" -l "$WORK/pg.log" -w start >/dev/null
B="psql -X -v ON_ERROR_STOP=1 -q -h 127.0.0.1 -p $PORT -U blades"
$B -d postgres -c "CREATE DATABASE blades"
$B -d blades <<'SQL'
CREATE TABLE users (id uuid PRIMARY KEY, data jsonb NOT NULL);
CREATE TABLE guild_manage_log (id bigserial PRIMARY KEY, msg text);
CREATE SEQUENCE loose_seq;
CREATE TABLE arena_match_results (id uuid PRIMARY KEY, opponent uuid);
CREATE FUNCTION character_name_norm(n text) RETURNS text LANGUAGE sql IMMUTABLE AS 'select lower(n)';
CREATE TABLE users_backup_20260911 AS SELECT * FROM users;
SQL
export ARENA_RUNTIME_DB_PASSWORD='local-test-pw-xyz'
$B -d blades -f "$WORK/roles.sql"
$B -d blades -f "$WORK/roles.sql"   # idempotent
echo "owners after convergence:"
$B -d blades -At -c "select relname||' '||relowner::regrole from pg_class where relnamespace='public'::regnamespace and relkind in ('r','S') order by 1" -c "select proname||' '||proowner::regrole from pg_proc where pronamespace='public'::regnamespace"
export PGPASSWORD="$ARENA_RUNTIME_DB_PASSWORD"
R="psql -X -v ON_ERROR_STOP=1 -At -h 127.0.0.1 -p $PORT -U arena_runtime -d blades"
$R -c "select session_user||' as '||current_user"
$R -q <<'SQL'
INSERT INTO users VALUES (gen_random_uuid(), '{}');
UPDATE users SET data = '{"a":1}';
INSERT INTO guild_manage_log (msg) VALUES ('x');
SELECT nextval('loose_seq');
BEGIN; LOCK TABLE arena_match_results IN SHARE ROW EXCLUSIVE MODE; COMMIT;
SELECT pg_advisory_xact_lock(7262, 1);
CREATE TABLE IF NOT EXISTS character_xp_bank (id uuid PRIMARY KEY);
CREATE INDEX IF NOT EXISTS arena_match_results_by_opponent ON arena_match_results (opponent);
CREATE OR REPLACE FUNCTION character_name_norm(n text) RETURNS text LANGUAGE sql IMMUTABLE AS 'select lower(trim(n))';
ALTER TABLE arena_match_results ADD COLUMN IF NOT EXISTS rounds_won int NOT NULL DEFAULT 0;
DELETE FROM users;
SQL
echo "runtime DML + startup DDL: ok"
$R -c "select 'new table owner: '||tableowner from pg_tables where tablename='character_xp_bank'"
PGPASSWORD=wrong psql -X -h 127.0.0.1 -p $PORT -U arena_runtime -d blades -c 'select 1' >/dev/null 2>&1 && { echo "FAIL: wrong password accepted"; exit 1; } || echo "denied: wrong password"
for q in "COPY (select 1) TO PROGRAM 'true'" "select pg_read_file('/etc/hosts')" "CREATE ROLE evil" "ALTER SYSTEM SET work_mem='1MB'" "CREATE DATABASE evil" "ALTER ROLE blades PASSWORD 'x'"; do
  if $R -c "$q" >/dev/null 2>&1; then echo "FAIL: runtime could: $q"; exit 1; fi
  echo "denied: $q"
done
echo "ALL OK"
