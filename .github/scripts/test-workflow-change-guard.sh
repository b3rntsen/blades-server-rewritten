#!/usr/bin/env bash
# Tests for workflow-change-guard.sh against a fake `gh` that serves canned API
# responses. No network, no token. Each case states the verdict it expects;
# a guard that passes everything, or fails everything, fails this file.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GUARD="$HERE/workflow-change-guard.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# The fake gh: `gh api [--paginate] [-X METHOD] PATH`. A response is the file
# $FIX/<path with every non-alphanumeric run turned into _>.json; no file means
# the API call fails. DELETEs are logged to $FIX/deleted.
mkdir -p "$WORK/bin"
cat > "$WORK/bin/gh" <<'FAKE'
#!/usr/bin/env bash
shift  # "api"
method=GET
while [[ $# -gt 1 ]]; do
  case "$1" in
    --paginate) shift ;;
    -X) method="$2"; shift 2 ;;
    *) break ;;
  esac
done
path="${1%%\?*}"
if [[ "$method" == DELETE ]]; then echo "$path" >> "$FIX/deleted"; echo '[]'; exit 0; fi
key="$(printf '%s' "$path" | sed -E 's/[^A-Za-z0-9]+/_/g')"
[[ -f "$FIX/$key.json" ]] || { echo "fake gh: no fixture for $path" >&2; exit 1; }
cat "$FIX/$key.json"
FAKE
chmod +x "$WORK/bin/gh"

pass=0; failed=0
REPO=o/r
P=repos_o_r

# new_case <files-json> [labels-json] [events-json] [role]
new_case() {
  FIX="$WORK/case$((pass + failed))"
  rm -rf "$FIX"; mkdir -p "$FIX"
  local files="$1" labels="${2:-[]}" events="${3:-[]}" role="${4:-}"
  echo "{\"changed_files\": $(jq length <<< "$files")}" > "$FIX/${P}_pulls_7.json"
  printf '%s' "$files"  > "$FIX/${P}_pulls_7_files.json"
  printf '%s' "$labels" > "$FIX/${P}_issues_7_labels.json"
  printf '%s' "$events" > "$FIX/${P}_issues_7_events.json"
  if [[ -n "$role" ]]; then
    for who in alice mallory; do
      local r="$role"; [[ "$who" == mallory ]] && r="write"
      echo "{\"permission\":\"x\",\"role_name\":\"$r\"}" > "$FIX/${P}_collaborators_${who}_permission.json"
    done
  fi
  export FIX
}

# expect <pass|fail> <name> [EVENT_ACTION]
expect() {
  local want="$1" name="$2" action="${3:-opened}" got rc=0
  out="$(PATH="$WORK/bin:$PATH" REPO=$REPO PR_NUMBER=7 EVENT_ACTION="$action" bash "$GUARD" 2>&1)" || rc=$?
  got=$([[ $rc -eq 0 ]] && echo pass || echo fail)
  if [[ "$got" == "$want" ]]; then
    pass=$((pass + 1)); echo "  ok   $name"
  else
    failed=$((failed + 1)); echo "  FAIL $name: wanted $want, got $got"; sed 's/^/       | /' <<< "$out"
  fi
}

f() { jq -nc --arg n "$1" --arg s "${2:-modified}" --arg p "${3:-}" --arg prev "${4:-}" \
  '{filename:$n,status:$s} + (if $p != "" then {patch:$p} else {} end) + (if $prev != "" then {previous_filename:$prev} else {} end)'; }
files() { jq -sc . <<< "$*"; }
LBL='[{"name":"workflow-change-reviewed"}]'
labeled_by() { jq -nc --args '[$ARGS.positional[] | {event:"labeled",actor:{login:.},label:{name:"workflow-change-reviewed"}}]' "$@"; }

echo "workflow-change-guard:"

new_case "$(files "$(f web/app.ts)" "$(f scripts/x.py)")"
expect pass "ordinary code changes pass"

new_case "$(files "$(f .github/workflows/ci.yml)")"
expect fail "a modified workflow fails without the label"

new_case "$(files "$(f .github/workflows/evil.yml added)")"
expect fail "an added workflow fails"

new_case "$(files "$(f .github/workflows/ci.yml removed)")"
expect fail "a deleted workflow fails (it may be the check)"

new_case "$(files "$(f docs/ci.yml renamed '' .github/workflows/ci.yml)")"
expect fail "renaming a workflow out of .github/workflows fails"

new_case "$(files "$(f .github/actions/setup/action.yml added)")"
expect fail "a composite action fails"

new_case "$(files "$(f .github/scripts/workflow-change-guard.sh)")"
expect fail "changing the guard itself fails"

new_case "$(files "$(f .vscode/extensions.json added)")"
expect fail "anything under .vscode/ fails"

new_case "$(files "$(f web/.vscode/settings.json added)")"
expect fail "a nested .vscode/ fails"

new_case "$(files "$(f tools/tasks.json added $'@@ -0,0 +1 @@\n+{"tasks":[{"runOptions":{"runOn":"folderOpen"}}]}')")"
expect fail "a tasks.json adding runOn fails"

new_case "$(files "$(f foo.code-workspace modified $'@@ -1 +1,2 @@\n {\n+ "runOn": "folderOpen"')")"
expect fail "a .code-workspace adding runOn fails"

new_case "$(files "$(f web/settings.json modified $'@@ -1 +1 @@\n-"a": 1\n+"a": 2')")"
expect pass "a settings.json without runOn passes"

new_case "$(files "$(f web/settings.json modified $'@@ -1,2 +1 @@\n-"runOn": "folderOpen"\n "a": 1')")"
expect pass "REMOVING a runOn line passes"

new_case "$(files "$(f tools/tasks.json modified)")"
expect fail "a tasks.json with no visible diff fails closed"

new_case "$(files "$(f .github/workflows/ci.yml)")" "$LBL" "$(labeled_by alice)" admin
expect pass "labelled by an admin passes"

new_case "$(files "$(f .github/workflows/ci.yml)")" "$LBL" "$(labeled_by alice)" maintain
expect pass "labelled by a maintainer passes"

new_case "$(files "$(f .github/workflows/ci.yml)")" "$LBL" "$(labeled_by mallory)" admin
expect fail "labelled by a write-role collaborator fails"

new_case "$(files "$(f .github/workflows/ci.yml)")" "$LBL" "$(labeled_by alice mallory)" admin
expect fail "the LATEST labeller counts (admin, then removed and re-added by a writer)"

new_case "$(files "$(f .github/workflows/ci.yml)")" "$LBL" "$(labeled_by alice)" admin
expect fail "a push after review fails" synchronize
deleted="$(cat "$FIX/deleted" 2>/dev/null || true)"
if [[ "$deleted" == "repos/o/r/issues/7/labels/workflow-change-reviewed" ]]; then
  pass=$((pass + 1)); echo "  ok   ...and removes the stale label"
else
  failed=$((failed + 1)); echo "  FAIL ...and removes the stale label: deleted='$deleted'"
fi

new_case "$(files "$(f web/app.ts)")" "$LBL" "$(labeled_by alice)" admin
expect pass "a push to a PR with no guarded change keeps passing" synchronize
if [[ ! -e "$FIX/deleted" ]]; then
  pass=$((pass + 1)); echo "  ok   ...and leaves labels alone"
else
  failed=$((failed + 1)); echo "  FAIL ...and leaves labels alone"
fi

new_case "$(files "$(f .github/workflows/ci.yml)")" "$LBL" "$(labeled_by alice)"
expect fail "an unreadable role fails closed"

new_case "$(files "$(f .github/workflows/ci.yml)")"
rm "$FIX/${P}_pulls_7_files.json"
expect fail "an API failure fails closed"

new_case "$(files "$(f web/app.ts)")"
echo '{"changed_files": 3001}' > "$FIX/${P}_pulls_7.json"
expect fail "files the API did not list fail closed"

# A file name that is itself a workflow command must not be obeyed.
new_case "$(files "$(f $'.github/workflows/x\n::error::forged' added)")"
out="$(PATH="$WORK/bin:$PATH" REPO=$REPO PR_NUMBER=7 bash "$GUARD" 2>&1 || true)"
if grep -q '^::stop-commands::' <<< "$out" && ! grep -q '^::error::forged' <<< "$out"; then
  pass=$((pass + 1)); echo "  ok   a hostile file name is printed inert"
else
  failed=$((failed + 1)); echo "  FAIL a hostile file name is printed inert"; sed 's/^/       | /' <<< "$out"
fi

echo "$pass passed, $failed failed"
[[ $failed -eq 0 ]]
