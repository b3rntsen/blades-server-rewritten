#!/usr/bin/env bash
# Tests the arena-migrate entrypoint's BRANCH LOGIC, with psql mocked.
#
# Why mocked rather than a real Postgres: the dangerous decision in that script
# is which branch it takes, not whether psql can run SQL. Get the branch wrong
# on an established database and it re-runs 16 non-idempotent migrations against
# production. That decision is pure control flow and can be tested exactly,
# offline, in a second — so there is no excuse for shipping it untested.
#
# The script under test is extracted from docker-compose.arena.yml rather than
# copied here, so this cannot pass against a stale duplicate.
#
#   deploy/test-migrate-ledger.sh

set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
PASS=0; FAIL=0

extract_script() {
  python3 - "$WORK/migrate.sh" <<'PY'
import re, sys
y = open('docker-compose.arena.yml').read()
m = re.search(r'    entrypoint:\n      - sh\n      - -c\n      - \|\n(.*?)\n    restart: "no"', y, re.S)
if not m:
    sys.exit("could not find the arena-migrate entrypoint in docker-compose.arena.yml")
body = [l[8:] if l.startswith(' ' * 8) else l for l in m.group(1).split('\n')]
# compose escapes a literal $ as $$; undo that to get the real shell script.
open(sys.argv[1], 'w').write('\n'.join(body).replace('$$', '$'))
PY
  # The script reads /migrations, which is the container's bind mount and does
  # not exist here. Without this every loop iterates zero times and the tests
  # pass vacuously — they did, on the first run, which is why this line has a
  # comment on it.
  sed -i.bak "s#/migrations#$PWD/migrations#g" "$WORK/migrate.sh"
}

# $1 = "yes"/"no" users table exists, $2 = space-separated versions already in the ledger
make_mock_psql() {
  local users_exists="$1" ledger="$2"
  cat > "$WORK/psql" <<EOF
#!/usr/bin/env bash
# Mock psql. Logs -f applications, answers the three queries the script asks.
LEDGER="$WORK/ledger"
for a in "\$@"; do
  if [[ "\$prev" == "-f" ]]; then echo "\$a" >> "$WORK/applied"; exit 0; fi
  prev="\$a"
done
q=""
prev=""
for a in "\$@"; do
  [[ "\$prev" == "-c" ]] && q="\$a"
  prev="\$a"
done
case "\$q" in
  *"CREATE TABLE IF NOT EXISTS schema_migrations"*) exit 0 ;;
  *"SELECT count(*) FROM schema_migrations"*)  wc -l < "\$LEDGER" | tr -d ' ' ; exit 0 ;;
  *"to_regclass"*) [[ "$users_exists" == "yes" ]] && echo "users"; exit 0 ;;
  *"SELECT 1 FROM schema_migrations WHERE version"*)
      v=\$(sed -E "s/.*version = '([^']*)'.*/\1/" <<< "\$q")
      grep -qx "\$v" "\$LEDGER" && echo 1; exit 0 ;;
  *"INSERT INTO schema_migrations"*)
      v=\$(sed -E "s/.*VALUES \('([^']*)'\).*/\1/" <<< "\$q")
      grep -qx "\$v" "\$LEDGER" || echo "\$v" >> "\$LEDGER"; exit 0 ;;
esac
exit 0
EOF
  chmod +x "$WORK/psql"
  : > "$WORK/applied"
  : > "$WORK/ledger"
  for v in $ledger; do echo "$v" >> "$WORK/ledger"; done
}

check() {
  local name="$1" want_applied="$2" want_ledger="$3"
  local got_applied got_ledger
  got_applied=$(wc -l < "$WORK/applied" | tr -d ' ')
  got_ledger=$(wc -l < "$WORK/ledger" | tr -d ' ')
  if [[ "$got_applied" == "$want_applied" && "$got_ledger" == "$want_ledger" ]]; then
    echo "  ok    $name (applied=$got_applied ledger=$got_ledger)"; PASS=$((PASS+1))
  else
    echo "  FAIL  $name — applied=$got_applied want $want_applied; ledger=$got_ledger want $want_ledger"
    FAIL=$((FAIL+1))
  fi
}

extract_script
TOTAL=$(ls -d migrations/*/ 2>/dev/null | wc -l | tr -d ' ')
echo "migrations on disk: $TOTAL"
ALL=$(for d in migrations/*/; do basename "$d"; done)

echo
echo "1. Fresh database — no users table, empty ledger"
echo "   (must apply every migration)"
make_mock_psql no ""
PATH="$WORK:$PATH" sh "$WORK/migrate.sh" > "$WORK/out1" 2>&1
check "fresh applies all" "$TOTAL" "$TOTAL"

echo
echo "2. ESTABLISHED database — users table exists, ledger empty"
echo "   (must adopt and apply NOTHING — this is the production case, and"
echo "    applying here would re-run 16 non-idempotent migrations on live data)"
make_mock_psql yes ""
PATH="$WORK:$PATH" sh "$WORK/migrate.sh" > "$WORK/out2" 2>&1
check "established adopts, applies nothing" "0" "$TOTAL"
grep -q "adopting, applying nothing" "$WORK/out2" \
  && { echo "  ok    says so out loud"; PASS=$((PASS+1)); } \
  || { echo "  FAIL  no adoption message"; FAIL=$((FAIL+1)); }

echo
echo "3. Ledger populated except the newest — must apply exactly one"
NEWEST=$(echo "$ALL" | sort | tail -1)
ALL_BUT_NEWEST=$(echo "$ALL" | sort | sed '$d' | tr '\n' ' ')
make_mock_psql yes "$ALL_BUT_NEWEST"
PATH="$WORK:$PATH" sh "$WORK/migrate.sh" > "$WORK/out3" 2>&1
check "applies only the new one" "1" "$TOTAL"
grep -q "applying $NEWEST" "$WORK/out3" \
  && { echo "  ok    applied $NEWEST specifically"; PASS=$((PASS+1)); } \
  || { echo "  FAIL  did not apply $NEWEST"; FAIL=$((FAIL+1)); }

echo
echo "4. Fully-applied ledger — must be a no-op"
make_mock_psql yes "$(echo "$ALL" | tr '\n' ' ')"
PATH="$WORK:$PATH" sh "$WORK/migrate.sh" > "$WORK/out4" 2>&1
check "no-op when up to date" "0" "$TOTAL"

echo
if [[ $FAIL -gt 0 ]]; then echo "FAILED — $FAIL failure(s), $PASS passed"; exit 1; fi
echo "passed — $PASS checks"
